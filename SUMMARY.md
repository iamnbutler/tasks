# One-command server upgrade loop: `make serve` / `make restart` / `tasks reload`

The server now has the upgrade loop the app already had. `tasks reload` (alias
`restart`) **builds first, reports what is in flight, gates on it, optionally
drains, SIGTERMs the old process, starts the new binary, and waits for the new
pid to answer** — then says which migrations that boot applied. The order is
the load-bearing part: a failed build costs nothing because nothing has been
signalled yet, and "did it come up?" and "did the schema move?" are answered by
the new process rather than assumed. Nothing in `reload` opens the store —
`Store::open` runs migrations, so a supervisor that opened the database would
apply the new schema before the new binary booted, masking exactly the failure
it exists to catch. Around it: a `GET /status` route that answers both halves
(pid, uptime, this boot's migrations; mode and in-flight work) in one call, a
`<data dir>/tasks.pid` discovery record so "which server, on what port, from
which binary" stops being a `ps` puzzle, SIGTERM handling in `serve` — which
previously handled only ctrl-c, so the standard restart signal killed in-flight
scouts outright — and `make serve` / `restart` / `status` / `stop` targets that
always build before they signal (`RELOAD=--when-idle` for extra flags).

By default a swap refuses while a scout or a running build is in flight, naming
both ways forward (`--when-idle` waits for a drain point, `--force` swaps
anyway); exit codes are distinct (3 busy, 4 drain timed out, 5 the swap did not
land) so scripts can branch without parsing prose. Draining pauses dispatch for
the wait and restores the mode only after the new server answers — without the
pause the dispatcher starts a fresh scout the moment one finishes and the wait
never terminates. An owed orchestrator turn is reported but never blocks: the
obligation and nudge loops keep producing input, and the answered watermark
means a restart mid-turn costs one turn that the next boot takes again. Queued
builds are deliberately not counted as in flight — durable intent survives a
restart, and counting it would make a healthy backlog read as a permanent
reason never to restart. Liveness is always re-derived from the OS via `ps`
(where a `Z` state is dead — the stopped server is routinely an unreaped child,
and `kill -0` succeeds on zombies while procps also reports out-of-range pids
as alive), so the pidfile is a hint and never a lock: a killed server leaves
nothing to clean up by hand. This is not a service manager — no supervision, no
restart-on-crash; `launchd`/`systemd` compose with it, pointed at `tasks serve`.

Tested with real binaries, real SQLite and real signals: `crates/tasks/tests/
reload.rs` covers the idle swap end to end, the refusal-then-`--force` path, a
drain that waits and restores the mode, a timed-out drain that restarts nothing
and leaves the mode as it found it, a failed build that leaves the server
untouched, a second server on one data dir refusing, a stale pidfile not
blocking a start, a graceful `stop`, and `reload` with nothing running being
just a start — plus unit tests for `Store::in_flight`, the migration diff, the
`/status` wire shape, the pidfile (zombies included) and every rendering path.
`cargo test --workspace` is green (~380 tests, doctests included), `cargo fmt
--check` and `cargo clippy --workspace --all-targets` clean. (`make test` needs
cargo-nextest, which was unavailable in this sandbox; `cargo test --workspace`
— what `make test-cargo` runs — was used instead, with `-j 2` to keep the
linker inside the sandbox's memory.) Verified by hand as well: a fresh data dir
reports applying all 19 migrations, a second swap reports "already current",
`serve.log` carries the same list, and `tasks stop` leaves no pidfile.

Complementary to #844 (`/version`): the liveness probe needs in-flight and
migration data, so this adds and probes `/status`; when `/version` lands it is
identity information `reload` can print alongside the `up: pid …` line. And
independent of #842: this makes the loss of in-flight work visible and
refusable, #842 makes it not happen — when it lands, `is_destructible()` is the
single place that decides what still needs draining.
