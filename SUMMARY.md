# Transcript view: collapse raw JSON walls, one "session started" row per init

The session transcript pane had two problems that made a real run hard to
read. First, every stream-json line the client couldn't parse rendered as a
flat, uncollapsed `Text` — and the server cuts any transcript line at 32 KB
(`truncate_line`, `crates/tasks/src/scout.rs`), which leaves the record
unparseable, so a single `Read` of a moderately large file turned into
thousands of characters of escaped JSON dominating the pane. Second, *every*
`"type":"system"` record mapped to the "session started" row, so a run showed
clusters of them claiming the session had restarted.

`app/Tasks/TranscriptView.swift` now carries all of the behavior change.
`TranscriptRecord.raw` gains a `truncated` flag, set by matching the
`[tasks: truncated ` marker the server appends to lines it cuts, and renders
through a new `RawLineView`: at or below 200 characters the line looks exactly
as before (short stderr notes shouldn't grow a disclosure arrow), and above it
the line collapses into a `DisclosureGroup` labelled `truncated record` /
`stderr` / `unparsed record` plus a one-line, 120-character preview of the
first physical line. `system` records now switch on `subtype`, with only
`init` producing the session-started row and everything else a compact
one-line caption. Tool-result summaries and tool-use inputs are capped at
4 000 characters at parse time with a "truncated" marker — SwiftUI builds a
`DisclosureGroup`'s content whether or not it's expanded, so collapsing alone
would not have saved the pane from a 100 KB `Text`. The wire format is
unchanged: the server already marks cut lines, so the client just reads the
mark. `crates/tasks/src/scout.rs` gets a doc comment stating that the marker
prefix is a contract the app matches on, and a unit test proving that cutting
a valid oversized `tool_result` record makes it stop parsing as JSON while
keeping the marker.

Verification: `cargo test --workspace` green, `cargo fmt --check` clean,
`cargo clippy --workspace --all-targets` reports nothing in the touched files.
The Swift has no CI (`app/` builds only under `make app` on macOS), so it was
checked with a Linux Swift 6.1 toolchain: `swiftc -parse` on the whole file,
plus a harness that compiles `TranscriptRecord`/`AssistantBlock` verbatim
against stub model types and asserts the parsing rules (truncated multi-KB
line → `.raw(truncated: true)`; unmarked junk and the session-cap stderr
notice → `.raw(truncated: false)`; `init` → `systemInit` with the model;
`compact_boundary` and subtype-less system → `systemNote`; 100 KB tool result
and tool input both capped at 4 035 characters; assistant text, thinking,
error results and the final result row unchanged), and the view structs
type-checked against a minimal SwiftUI shim. The pane still deserves a real
look under `make run` before merging, since layout is the one thing none of
that covers.
