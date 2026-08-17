An approved spec's review feedback reached the Builder in no form at all. It
lives on `spec_queue.feedback`; the build prompt was assembled from `specs` and
`tasks`, and `Spec` has no such column — so every required item that was not
*also* spec content was dropped by construction, which is why the ones that
went missing were uniformly documentation, naming and framing rather than
implementation. The fix is #917's applied one subsystem over.
`Builder::load_batch` now reads the queue entry per spec into a new
`BatchItem { spec, task, review_feedback }` — a struct rather than the
`(Spec, Task)` tuple it replaces, because the third field is the one that is
easy to lose — and `BatchItem::new` is the single place blank feedback becomes
`None`. `render_prompt` renders it as its own `## Review feedback on these
specs` section between the specs and the directions (the order the three
channels were written in, which is also why directions stay last as the later
word), attributed per spec with `### On spec N of M` subsections, because
unattributed feedback in a batch of three is a guess about which spec it
belongs to. The section requires each item to be accounted for in `SUMMARY.md`
under a `## Review feedback` heading — done, or declined with a reason — and
since the summary *is* the PR body, that accounting is in front of the reviewer
without anything being fetched. The queue entry is read at *prompt* time rather
than snapshotted onto the build row, so a batch a `watch_merges` unwind sent
back to `ready_to_build` is rebuilt under the same requirements; a spec with no
queue entry reads exactly as an approval that said nothing; and a batch nobody
left feedback on grows no heading at all, the same rule that keeps an undirected
build from growing an empty `## Directions`.

The other half is that nothing downstream checked. `brief.rs`'s `World.queue`
now holds the whole `SpecQueueEntry` instead of just its status, and a new
`review_feedback_line` rides a stranded build's brief only when that batch
carried feedback: two wordings, one saying the summary has the section and one
saying it does not and to read the diff. `summary_accounts_for_review_feedback`
is a **presence check** — it cannot tell a real accounting from a bare heading,
so it is reported as the build's own claim exactly like `Verification:`, and the
next-non-space-character rule keeps "Review feedback was helpful" in a sentence
from reading as one. It is deliberately a brief fact and not a fourth
`landing_section` carve-out — those three are all about whether a change can be
*verified* — and the line's own wording ends "on its own this is not a reason to
refuse the merge". Two smaller pieces close the same silent drop elsewhere: the
scout's `## Previous attempt` section now asks for each feedback item to be
accounted for in `SPEC.md`'s `### Notes`, declines included (it already called
the feedback a requirement and had nowhere for a refusal to land), and the GUI's
task-pane **Approve** button now carries `take_review_draft(cx)` instead of
passing `None`, so a human reviewer can approve *with* required changes at all
rather than only the orchestrator being able to via the API. It is not gated on
`has_text` — approve is the one exit that does not need text. `rationale`
deliberately does not follow this path: `review_spec` takes both, only
`feedback` is agent-facing, and a decision record addressed to the ledger has no
business in a VM. Docs state the rule once in each place that states it
(`ReviewRequest::feedback`, `Store::review_spec`, the orchestrator prompt's
`rationale`/`directions` paragraph and its `/spec-queue/{id}/review` endpoint
line, and a CLAUDE.md bullet after the `directions` one). Tests: three unit
tests in `builder.rs` (placement, per-spec attribution with a negative assertion
that a spec without feedback grows no subsection, the no-heading case, and the
accounting parser both ways), one in `brief.rs` (both wordings, silence without
feedback, blank == absent), one extended in `scout.rs`, and an end-to-end
`the_review_feedback_a_spec_was_approved_with_reaches_the_builder_agent` driving
a real VM and the real supervisor through `Store::review_spec` — the unit tests
pin what `render_prompt` writes, but only that one pins that the dispatcher goes
and *reads the feedback out of the store*, which is the half that was missing.
It uses a new `echo-prompt-builder-agent.sh` fixture, the Builder counterpart of
the Scout's, which echoes its whole prompt into `SUMMARY.md`; the summary comes
back on the build row, so what the agent was told is assertable from outside the
VM. No migration, no protocol change, no image rebuild — the prompt is
host-side, so this takes effect on the next build after the server restarts.

Verification: PASSED — `make test` (700 passed, 0 failed; 7 leaky, the documented expected ones; doctests included), `cargo clippy --workspace --all-targets` clean, `cargo fmt --all` applied, plus `make app-check` and `make app-test` (196 passed) for the GUI change. The GUI change itself lives inside a render closure, so no unit test covers it and a click was not observed — that takes a Mac.
