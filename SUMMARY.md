Builds now leave a transcript. `drain_build_events` consumed every
`BuildEvent::Progress` into a `debug!` and threw it away, so a build that ran
its whole budget and committed nothing left nothing behind but `exit_reason:
"no commits"` and two timestamps — nothing that said *why*. Rather than a
second implementation, the existing scout transcript machinery grows a second
owner: `transcript_lines` gains an exclusive arc (`session_id` xor `build_id`,
a CHECK plus two unique indexes, both sides keeping the ON DELETE CASCADE that
made transcript lifetime free), and the sink/writer/truncation code moves out of
`scout.rs` into a shared `crates/tasks/src/transcript.rs` that both dispatchers
use unchanged. Builds get `GET /builds/{id}/transcript?since=N` and the SSE tail
on exactly the session contract — same 32 KiB/8 MiB caps, same
`[tasks: truncated ` marker, same inclusive `since`, same real `LIMIT` — served
by owner-resolving wrappers over one shared pair of handlers. The builder
flushes the writer *before* the build row is finalized, including on the
timeout path where the drain future has already been cancelled, so a client
refetching on the outcome finds a complete transcript rather than a truncated
one. One line the server writes itself, `[tasks] builder agent exited with code
N`, puts the agent's exit status in the same ordered stream as the output that
explains it, and the orchestrator's prompt now names the build transcript as
the first thing to read when a build failed.

The wire type changed: `TranscriptLine.session_id` became
`owner: TranscriptOwner`, an internally-tagged enum with `session` and `build`
variants — two variants rather than one opaque id because they are two
resources behind two routes. `tasks-api` is strict and clients ship from this
repo, so this is a build error rather than runtime skew; `app-gpui` renders no
transcripts today, so nothing needed changing there, but any out-of-tree
consumer breaks. Note that `seq` restarts at 1 per owner, so clients paging
`?since=` must keep cursors per owner, not per task. Tests are real end to end:
a new builder integration test drives a fixture agent that talks on both pipes
and commits nothing, then reads the transcript back and asserts dense seqs,
stdout/stderr separation, the agent's stated reason, the exit-code line, and
that nothing leaked onto the spec's scout session; store tests cover per-owner
sequencing, arc enforcement and cascade, and a migration test that populates
the pre-0020 schema, migrates for real, and checks existing session
transcripts survived with their seqs intact. Two known gaps are deliberately
left alone: credentials in clone URLs can still reach a transcript (pre-existing
for scouts, #759, and the fix is one `redact()` call in `TranscriptSink::push`
that should change both owners at once), and builds still have no `usage`
column, so a build's token cost is visible inside its transcript but not on the
build row.
