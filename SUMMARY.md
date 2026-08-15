# Name new migrations for a UTC instant, and make a collision a red test

Migrations stop being numbered and start being timestamped. A new migration is
`crates/tasks/migrations/20260815030411_build_transcripts.sql` —
`YYYYMMDDHHMMSS` in UTC, digits only — written by a new
`make migration NAME=lower_snake_case` target. Two branches cut minutes apart
get different names without knowing about each other, which is the one property
"next free number" cannot have when the tree an agent was handed cannot see its
siblings: both look at 0023, both write 0024, and the collision only exists
after the merge, where it surfaces as a boot failure in a process that has
already taken the port. The change is purely additive — 0001–0023 keep their
versions, their checksums and their order, because sqlx records version *and*
checksum (so an applied migration can never be renamed) and a 14-digit stamp
sorts after any four-digit sequence number. No new migration ships here; there
is no schema change to make.

`MIGRATOR` moves out of `store.rs` into a new `crates/tasks/src/migrations.rs`,
which is where the convention is documented and where the guards live: no two
migrations share a version; every version is either a frozen legacy number or a
real UTC instant (so `0024_foo.sql` cannot quietly re-open the sequence);
every file in the directory actually made it into the embedded set (sqlx skips
a name it cannot parse *silently*, which is worse than a collision); a
legacy+timestamp pair applies in the right order through a real `Migrator`
against real SQLite; and an ISO-8601-style `20260815T030411_…` is rejected
rather than ignored, because sqlx parses the text before the first `_` as an
`i64`. Each of the first three was confirmed to fail red, with a message that
names the offending files and points at `make migration`. CLAUDE.md carries the
rule so the next agent reads it before copying the file next to it, and
`AppliedMigration::file_stem` gains a note plus a test pinning that its `{:04}`
is load-bearing for the legacy sequence and a no-op for a stamp.
