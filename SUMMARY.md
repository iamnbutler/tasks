# Scout checkpoints — salvage a dead run without ever calling it a spec

A Scout that died before writing `SPEC.md` lost its entire run: no checkpoint,
no partial credit, and the next attempt re-derived everything from zero. The
obvious fix — report the partial spec — is worse than the bug it fixes, because
a half-explored spec entering the review queue *looks finished*. This change
resolves that with two files carrying two meanings: `SPEC.md` still means "I
concluded", and a new `NOTES.md` means "here is what I have so far". The
supervisor polls `NOTES.md` and streams it back as `ScoutEvent::Checkpoint`
every 30s while the agent works — pushed, not pulled, because at the deadline
the host cancels by destroying the VM and there is nobody left to ask — and
reports `ScoutEvent::StoppedEarly` (a third terminal outcome, neither success
nor failure) when a run ends without a spec but with something written down.
Salvage lands in a new `scout_notes` table, the session becomes
`scout_stopped_early`, the task returns to `queued` and the attempt still
counts against the retry cap. There is no `Spec` row, no queue entry and no
review path for it; its only consumer is the next attempt's prompt, where it is
quoted as an explicitly unverified lead. The prompt now also asks scouts to keep
`NOTES.md` and tells them `SPEC.md` is not a checkpoint — which is what stops
"checkpoint early" from turning into "write a skeleton spec early".

The healthy path is deliberately untouched: a clean agent exit is taken at its
word and gets no structural audit at all, since losing a finished spec to a
heading-wording quibble would be a worse trade than the trap it prevents. Only a
non-zero exit reads `SPEC.md` sceptically, matching the template's sections
loosely by keyword, skipping fenced blocks, and counting template placeholders
as unfilled — and it was exactly the messy exit that previously completed a run
with any `SPEC.md` at all, however partial. Checkpoints are persisted as they
arrive rather than at the end (two of #825's four failures were "orphaned by
server restart", and in-memory notes would have died with the process), so the
startup orphan sweep can now tell a session that checkpointed from one that went
silent and mark it `scout_stopped_early` instead of `scout_failed`. A timed-out
run keeps its salvage but not a new name: `ScoutError::Timeout` and its
`"timed out"` exit reason are unchanged. Also included: `GET
/sessions/{id}/notes` (404 when there are none) and `Client::session_notes`
returning `Option`, migration `0020_scout_notes`, orchestrator nudges on
stopped-early sessions, and docs in `docs/clients.md` and `CLAUDE.md`. Three new
agent fixtures and eight new tests cover it, including the regression guard that
asserts `list_specs()` and `list_spec_queue()` are empty after an interrupted
run. `make test` is green (363 tests; the three LEAK-marked scout timeout tests
are expected), with clippy and rustfmt clean.
