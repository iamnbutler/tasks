# Build now: let a human skip scouting for tasks too simple to spec

`POST /tasks/{task_id}/build-now` writes a spec by hand for a task whose issue
body already *is* the specification, approves its queue entry in the same
transaction, and queues an ordinary Builder run over it — one call, because
from the human's side it is one decision. The spec is a first-class `Spec` row
with `session_id = NULL`, which is the tell that no Scout ran; `POST /builds`
and the whole Builder path are untouched, because a spec a human wrote is still
text and the Builder cannot tell the difference. What is skipped is not only
scouting but **review**: there is no independent artifact to rule on, so the
human writing the spec is the review, and a single `author_spec` decision row
(never an `approve`, which would imply a second opinion) carries the whole
judgment. The endpoint is human-only and refuses the orchestrator outright
rather than being charter-gated — authoring, approving and dispatching one's own
work with no second opinion anywhere in the loop is a materially different
autonomy from `dispatch_builds`, and if it is ever granted it wants its own
named capability.

Making `specs.session_id` nullable is the one genuinely delicate part. SQLite
cannot drop a `NOT NULL`, so it is the copy/drop/rename dance — with the wrinkle
that `specs` is a *parent*: `spec_queue` cascades off it and `build_specs`
references it with no `ON DELETE` action, so the implicit delete inside
`DROP TABLE specs` would cascade the review queue away and trip `build_specs`.
SQLite's own recipe says to turn foreign keys off around the swap, but
`PRAGMA foreign_keys` is a silent no-op inside a transaction and sqlx runs each
migration in one, and `-- no-transaction` would buy the pragma at the cost of a
half-migrated database on any failure. The children are therefore lifted into
temp tables, emptied, and re-inserted verbatim after the swap: same effect, still
atomic. A migration test seeds the full chain against real SQLite with foreign
keys enforced and pins every claim, including the surprising one — renaming
`specs_new` to `specs` while two tables hold references to a table that does not
exist at that instant works, and needs no `PRAGMA legacy_alter_table`.

Alongside: `Spec.session_id` and `EventPayload::SpecCreated.session_id` become
`Option`, `DecisionAction::AuthorSpec` joins the ledger vocabulary, the typed
client grows `build_task_now`, and the app gets a Build Now form in the inspector
whose draft is the *rationale* — the button stays inert until it has text, since
a one-click path to an unreviewed build is not worth the seconds it saves. The
inspector now also renders a task's latest spec whatever its state (it used to
render one only while in review) and reads `SPEC · SIMPLE · HUMAN-AUTHORED` when
there is no session behind it, rather than leaving a missing scout link to be
inferred. One thing outside the spec turned out to need fixing: `SpecCreated` was
unconditionally nudge-worthy, and the system prompt answers a spec landing by
reviewing it adversarially and rendering a verdict — so a human-authored spec
would have summoned the orchestrator to review work that is already approved and
already inside a build, with `auto_review_specs` live and `needs_revision` able
to send a `building` task back to `queued`. `SpecCreated` is now nudge-worthy only
when it names a session; the human's decision still reaches the conversation as
the approval that follows it.

Tests, lint and docs: 511 of 512 workspace tests pass, doctests pass, `cargo fmt
--all --check` and `cargo clippy --workspace --all-targets` are clean, and
`app-gpui` checks, clippies and runs its 99 unit tests. The single failure —
`tasks::reload when_idle_waits_for_the_drain_and_restores_the_mode`, which times
out at 60s — is environmental and predates this work: it was reproduced on a
clean tree by stashing every change. New coverage is five server tests (default
path with an empty body, `content` override, orchestrator 403 with every
plausibly relevant capability `live`, past-the-Scout refusal, nothing to build
from), three store tests (buildable end-to-end through `claim_next_queued_build`,
the legal source states, nothing written on refusal), one migration test, one
typed-client test, and one nudge assertion. `docs/clients.md` gains the action,
the nullable `session_id`, the new state edge and the `spec_created` payload
change; `CLAUDE.md` folds the endpoint into the Scout/Builder barrier rule and
the bulk-intake rule; and the orchestrator system prompt now names it as *not
yours*, so the model proposes the action to the human instead of discovering the
403.
