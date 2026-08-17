# Hold scout and build dispatch while GitHub is not answering

A Scout clones and a Builder clones, so work dispatched during a GitHub outage
dies at its first step — a *pre-agent setup failure*, which the strike rule
deliberately keeps charged, so one outage spent a three-spec batch three build
attempts of three for something no spec did. That rule is untouched here (waiving
pre-agent setup failures is the fix that looks equivalent and is not: a clone
against a base branch that is gone fails identically forever). What changes is
that the poller already knew and never told anyone. The new
`crates/tasks/src/github_health.rs` is that record: in memory, never a table —
it is a GitHub-owned fact with a timestamp on it, and the vm-pool precondition it
mirrors is in-memory too — written by the poll pass from **every** GitHub call it
makes (GraphQL and REST alike, via a new `run::GitHubWatch` that folds the
outcome in and announces the edge in one call, so an unannounced hold is not
something a call site can produce). The signal cannot be read off `poll_once`'s
return value, because a failed fetch is logged and skipped so one repo cannot
stall intake for the others, so the pass returns `Ok` through an outage that
failed every call in it. Whether a failure counts is decided **structurally**,
off a new `GhError::is_unavailable` — 5xx, or a request that never got a response
— and never off the message text, the same rule `FailureClass` follows; `429` and
every other `4xx` are excluded, because they are GitHub *answering* and a hold on
one would clear from nowhere. `GhError::Rest` becomes a struct variant carrying
the `StatusCode` so that decision has a field to read, with its rendered text
byte-identical (a test pins it).

Both dispatchers now read one `github_hold` predicate: `top_up` returns early
right after the mode check (a stop before the queue, not a filter over it —
skipping held work would just pick a different victim), and the build lane puts
it in the `Ok(Mode::Play)` match guard **ahead of `claim_next_queued_build`**,
since claiming would flip the build `queued → running` and drag its batch to
`building` on every tick of the outage. Three rules keep a hold from becoming a
silent stall, and each is a way this could go permanently wrong: absence of
evidence never holds (a tokenless server observes nothing at all), only a fresh
success clears one (a 404 on one PR is not GitHub coming back), and a hold nobody
is refreshing expires — generously, at 10 × `TASKS_POLL_INTERVAL` floored at ten
minutes, because during an outage the poller's own requests are the slow kind and
a tight window would expire the hold *during* the outage it was set for. The
window is bound at construction, so the scout loop, the build lane and `/status`
cannot disagree about whether a hold is in force. Holding is safe here in a way
it usually is not: `POST /builds` still records the request, queued work stays
queued, nothing is charged an attempt, and the batch takes the lane on the tick
after GitHub answers. The edge is announced once as an event-log `Note` (not
nudge-worthy — the orchestrator cannot fix GitHub) and the hold is reported for
as long as it lasts on `GET /status` (`ServerStatus.github`, `#[serde(default)]`
for the same reload-skew reason as `images`), `tasks status` and the Server
window — both silent when there is no hold, and both printing the age of the
outage beside the age of the last observation, since that gap is the difference
between a hold somebody is still refreshing and one about to expire on its own.
Also included: the documented load-bearing bullet in `CLAUDE.md`, and a one-line
fix to a pre-existing race in `pause_blocks_new_dispatches` (`top_up` reads the
mode and *then* queries the queue) that the two new dispatch tests surfaced.

Verification: PASSED — `make test` (732 tests, 0 failures; the 7 LEAKs are the
documented pre-existing scout-timeout ones), `make app-test` (197 tests),
`cargo clippy --workspace --all-targets` clean, `cargo fmt --check` clean in both
the workspace and `app-gpui`.
