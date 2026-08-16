# Build now: let a human skip scouting for tasks too simple to spec

`POST /tasks/{task_id}/build-now` writes a spec by hand for a task whose issue
body already *is* the specification, approves its queue entry in the same
transaction, and queues an ordinary Builder run over it — one call, because
from the human's side it is one decision. The spec is a first-class `Spec` row
with `session_id = NULL`, which is the tell that no Scout ran; `POST /builds`
and the whole Builder path are unchanged, because a spec a human wrote is
still text and the Builder cannot tell the difference. What is skipped is not
only the scouting but the **review**: there is no independent artifact to rule
on, so the human writing the spec is the review, and a single `author_spec`
decision row (never an `approve`, which would imply a second opinion) carries
the whole judgment. The endpoint is **human-only and refuses the orchestrator
outright** rather than being charter-gated — authoring, approving and
dispatching its own work with no second opinion anywhere in the loop is a
materially different autonomy from `dispatch_builds`, and if it is ever
granted it wants its own named capability. The orchestrator's system prompt
now lists the endpoint as explicitly *not* its own, so it proposes the action
to the human instead of discovering the 403.

The load-bearing part is the migration that makes `specs.session_id` nullable.
SQLite cannot drop a `NOT NULL`, so it is the copy/drop/rename dance — but
unlike the last one, `specs` is a *parent*: `spec_queue.spec_id` cascades off
it and `build_specs.spec_id` references it with no `ON DELETE` action at all,
so `DROP TABLE specs` with foreign keys enforced would cascade the queue away
and trip `build_specs`. SQLite's own recipe says to suspend foreign keys
around the swap, but `PRAGMA foreign_keys` is a silent no-op inside a
transaction and sqlx runs each migration in one; `-- no-transaction` would buy
the pragma at the cost of a half-migrated database on any failure. The
children are therefore lifted into `CREATE TEMP TABLE …_carry`, deleted, and
re-inserted verbatim after the swap — same effect, still atomic. A test seeds
a spec, its approved queue entry and a build batch into a database migrated up
to that point, asserts foreign keys really are enforced, and then applies the
swap, so the guard is exercised rather than assumed; it also pins the thing
expected to blow up and did not, `ALTER TABLE specs_new RENAME TO specs` while
two tables hold keys into a `specs` that no longer exists. Note the migration
is named for a UTC instant (`20260816010503_human_authored_specs.sql`) rather
than `0024_`, because the sequence closed in `62d5f1c` and a guard test now
makes reopening it red.

Alongside: `Spec.session_id` and `EventPayload::SpecCreated.session_id` become
`Option`, with the provenance contract documented on the field; `files_touched`
stays `[]` on a hand-written spec, because `brief.rs` derives overlap facts
from that list and an invented one would feed the brief a lie rather than an
omission. `author_spec` refuses any task not in `backlog` or `queued` — the two
states from which no Scout has run and none is running — and every refusal
lands before the transaction opens. `Queued` is allowed even though it can
already carry a spec, since a `needs_revision` verdict returns a task there and
the human who read that feedback may write the spec themselves. The two store
calls in the handler are deliberately not merged: if `create_build` fails the
spec is still approved and the task sits in `ready_to_build`, recoverable with
a plain `POST /builds`, which is a better place to land than having silently
discarded what the human wrote. The typed client gains `build_task_now`, and
the app gains a **Build Now** form in the inspector for `backlog`/`queued`
tasks whose draft is the *rationale* — the button is inert until it has text,
because a one-click path to an unreviewed build is not worth the seconds it
saves — plus provenance in the spec header, which now reads
`SPEC · SIMPLE · HUMAN-AUTHORED` and renders the latest spec whatever its
review state rather than only while one is pending. Docs updated in
`docs/clients.md` and `CLAUDE.md`.

## Testing

`cargo fmt --all --check` and `cargo clippy --workspace --all-targets` are
clean, as are `app-gpui`'s own `cargo fmt`/`cargo clippy`. 495 workspace tests
and the doctests pass. New coverage: six server tests (default path, a
bodyless POST, `content` override, orchestrator 403, past-the-Scout refusal,
nothing to build from), a charter test that the 403 holds with every plausible
capability `live` and that the same call as the human goes through, three
store tests (buildable end-to-end through `claim_next_queued_build`, the legal
source states, nothing written on refusal), the migration test above, and a
typed-client test.

One pre-existing failure is unrelated and left alone:
`tasks::reload when_idle_waits_for_the_drain_and_restores_the_mode` times out
at 60s. It was verified to do so on the clean tree — every change stashed —
so it is environmental and predates this work.

`app-gpui` is verified by `cargo check` and `cargo clippy`, not by `cargo
test`: it is not a workspace member and `make app` refuses off macOS. It does
compile on Linux once fontconfig is reachable (a rootless `apt-get download` +
`dpkg-deb -x` of `libfontconfig-dev` and `pkgconf` plus `PKG_CONFIG_PATH` /
`PKG_CONFIG_SYSROOT_DIR` is enough), but its test target still fails at link
time on `-lxcb`/`-lxkbcommon`, which is an environment limit rather than a
code one. The GUI change is small and type-checked; it has not been run.
