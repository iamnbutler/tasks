Agent output reached `transcript_lines` verbatim, so an agent that ran `git
remote -v` — or any git command that echoes its remote in an error — wrote the
live `https://x-access-token:<token>@github.com/…` clone URL the server handed
its VM into a durable table served by `GET /sessions/{id}/transcript` and its
SSE tail. `builder::redact` already did exactly the right thing, but it was
only wired to the server's own error strings. This lifts it into
`crates/tasks/src/redact.rs` (algorithm unchanged, its test moved with it, plus
allocation-free `redact_line`/`redact_owned` for the hot path and a test
pinning idempotency) and calls it on the **write** path in
`Store::append_transcript_lines` — the single choke point every transcript
producer passes through, including the Builder sink #825 adds. The row and the
broadcast copy are built from the same scrubbed text, so an SSE tail and a
catch-up read see identical bytes. `TranscriptSink::push` scrubs again *before*
`truncate_line`: a 32 KiB cut landing inside `x-access-token:<token>@` would
otherwise strand a token prefix with no `@` behind it, unrecognisable as a
credential to anything downstream. `Scout::finalize_failed` scrubs `reason`
before it reaches `sessions.exit_reason`, the event log and the log line, and
the Builder's `BuildEvent::Progress` debug log is scrubbed too — a log file is
a file. Scrubbing on write rather than read means the token never reaches the
database, so a copy of `tasks.db` is covered along with the API; the cost is
that the raw bytes are unrecoverable, accepted because `redact` only rewrites
the userinfo of a URL authority and its tests pin that a path `@` and an
uncredentialed URL are untouched.

Rows already on disk can hold a token that is probably still valid, so they are
swept once. Migration `0020_redact_transcripts.sql` creates
`pending_maintenance` and inserts a `redact_transcripts` marker;
`run_pending_maintenance` (called after `MIGRATOR.run` in both constructors)
consumes markers it recognises and deletes them, so the scan happens on one
boot and never again, while an unrecognised marker is logged and left in place
— it means a newer binary wrote the database. `sweep_transcript_credentials`
rewrites matching rows through the same `redact` the write path uses, in
batched transactions with keyset pagination; `LIKE '%://%@%'` only narrows the
scan, `redact` still decides per row. A SQL-only sweep was rejected: SQLite has
no regex, so it would have meant a second implementation of the redaction rules
that could disagree with the first. The sweep is a mitigation, not an undo — it
can't un-serve anything already read over the API, so it also warns the
operator to rotate `GITHUB_TOKEN` if it changed anything. Tests cover the
module (redaction, idempotency, borrow behaviour), the sink (a credential
straddling the line cap, with the truncate-first trap asserted explicitly), the
store (write path scrubs row + broadcast + read-back, the sweep rewrites
pre-fix rows while leaving a path `@` alone, is one-shot, pages past its batch
size, and preserves unknown markers), and end to end: a real vm-pool, the real
scout-supervisor and a stub agent that echoes a credentialed URL on stdout, in
a stream-json record and on stderr, asserting the token is absent from the
transcript, from the API response, and from `tasks.db` **and** `tasks.db-wal`
— with the scrubbed form asserted present so the negative assertions can't pass
by reading nothing. `docs/clients.md` documents the scrub as client-visible
behaviour, since `app-gpui` renders lines verbatim and operators will see
`***@github.com`.

Two notes. The migration is numbered **0020**, not the 0017 the spec named:
0017 and 0018 are taken by in-flight branches (#824, #827) and main is at 0019,
so 0017 would have collided into a duplicate migration version. `make test` was
not run — `cargo-nextest` isn't installed in this environment — but
`cargo fmt --all`, `cargo clippy --workspace --all-targets` and `make test-cargo`
(the full workspace suite plus doctests) are all green.
