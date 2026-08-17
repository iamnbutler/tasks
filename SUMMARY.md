# Build: #918, #924, #925

Three approved specs, implemented in order.

**Remove the Home section and the whole briefing subsystem (#918).** The view,
`crates/tasks/src/briefing.rs`, `GET /briefings`, the client method, the
`Services::briefings` field, the `Briefing`/`BriefingSection`/`BriefingStatus`
wire types, the `briefing_updated` event and the three `BRIEFING_*` env vars
are all gone — the server no longer spends a Claude Code run per TTL window on
prose nothing reads. The load-bearing part is the migration rather than the
deletions: `EventPayload` is a strict internally-tagged enum and
`Store::event_from_row` deserializes it with a hard `?`, so removing
`BriefingUpdated` while `briefing_updated` rows are still in the `events` table
would make `GET /events` and `/events/stream` return 500 **forever** — the
Activity feed would die permanently on every database that ever generated a
briefing, and no later pass repairs it. `20260817135101_remove_briefings.sql`
therefore drops the `briefings` table *and* deletes those rows (the technique
and the precedent for editing the log to retire a vocabulary are already in
`0006_event_vocabulary.sql`), and
`migrations::tests::removing_briefings_takes_their_events_with_them` pins it —
seeding a real pre-migration database, opening it through `Store::open`, and
asserting the feed still reads and the surviving seqs are 1 and 3, a gap rather
than a renumbering. Without the `DELETE` that test fails on the `all_events()`
line, which is the whole reason to write it. **The one thing to notice on first
launch is that the ⌘-numbers shifted** rather than leaving a gap: ⌘1 Tasks, ⌘2
Queue, ⌘3 Activity, ⌘4 Chat, with Tasks becoming the default section via a
named `Section::DEFAULT` that `Workspace::new` and the #902/#914 focus test both
read. The removal is recoverable from git history, which is now where the only
read-only agent shape in the system lives (`BRIEFING_CMD`'s
gh/curl/git-log/git-diff allowlist, and `briefing.rs`'s `split_command` +
single-flight + cooldown machinery); `docs/plans/2026-08-11-home-briefings.md`
is kept for the same reason.

**Refuse a live vm-pool socket, and answer `--help` before any side effect
(#924).** `Service::run` unconditionally `remove_file`d the socket path and then
bound it, so a second daemon silently displaced a live one: the first went on
listening on an unlinked inode — healthy, `pgrep`-able, resolvable by `lsof`,
and unreachable forever — while the server reconnected to the path, found the
new pool, and handed it the queued work. That is now one `connect` before the
unlink (`vm_pool_service::bind_socket`): something answers ⇒ refuse and name the
path, the connection is refused ⇒ a dead daemon's leftover, unlink it and come
up, which is the recovery that used to need a human with `rm`. Every unreadable
answer counts as occupied, because a wrong refusal costs one error message and
one restart while a wrong takeover costs the incumbent every VM it holds; a path
that exists and is *not* a socket is refused rather than deleted. It lives in
vm-pool so both entry points get it and no app vocabulary crosses the boundary.
Separately, `tasks vm-pool --help` started the daemon, because `vm_pool()` took
no arguments at all and an unrecognized one therefore meant "proceed" — the help
check now runs in `dispatch()` ahead of every subcommand (deliberately not
per-function, which is one refactor away from being skipped again), each
subcommand has its own usage text, and `status`/`vm-pool`/`add-project` now
bail on arguments they used to drop in silence. **Operator-facing:** a vm-pool
start against an occupied socket now *fails* where it used to succeed. That is
the fix, but a script that restarts vm-pool by just launching a new one will see
it as new breakage — the correct sequence is stop, then start, and the error
message says so. The issue asked for "a real clap subcommand definition"; there
is no clap anywhere in this workspace, so the *invariant* was implemented in the
idiom the file already uses, and `tests/cli.rs` is written against the behaviour
rather than the mechanism.

**A vm-pool restart no longer charges a strike (#925).** `BuilderError::StreamClosed`
and `ScoutError::StreamClosed` were classified `FailureClass::Verdict`, four
lines below the comment explaining why `Egress` is `Transport`. Both are raised
when the vm-pool event stream ends — the daemon going away, which is routine
maintenance (this document says to restart vm-pool *ahead* of the server), not a
judgement on the work. The builder half was the live bug: `Builder::conclude`
reads `failure_class()` straight into `Strike::for_class`, so every vm-pool
restart that caught a build in flight charged the whole batch, and three of them
`blocked` specs that never failed to build. The scout half was already spared its
attempt by a *different* guard (`run::is_disconnect` returns before the class is
consulted), so moving it changes no behaviour — it keeps the two answers from
disagreeing, and `is_disconnect` now carries a doc comment saying why the two
must not be merged. `Client(_)` is deliberately left `Verdict` and commented as
such; the scout-side allocation case it points at is a real separate defect that
wants its own issue. The end-to-end test stands a Unix-socket relay the test owns
between the Builder's client and the *real* vm-pool service, holds a build open
with a gated agent, cuts the relay, and asserts `StreamClosed` is raised by the
code under test rather than constructed by the test — four rounds, one past
`MAX_BUILD_ATTEMPTS`, asserting the build owned a VM (so the store's own
never-started waiver is not what passes), the spec stays `approved`, and the
`DispatchBuild` obligation reads "0 of 3 attempts are charged". Reverting only
the builder arm makes it fail at round 3 with the spec `Blocked`, which is
exactly the reported failure.

Verification: PASSED — `make test` (nextest workspace + `cargo test --doc --workspace`), `make app-test`, `cargo clippy --workspace --all-targets`, `cargo fmt --all -- --check`
