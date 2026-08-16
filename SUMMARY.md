# Mode starts from a configured default, and only an upgrade resumes the old one

Mode stops being consulted at startup. Every boot now puts the pipeline into
`TASKS_DEFAULT_MODE` (default `pause`, read like every other config value, so
`.env` files work) and overwrites the stored value, which remains the live mode
for the rest of that process's life — `GET /mode`, `POST /mode` and the three
loops that read it every tick are all unchanged, and the schema is untouched.
The result is that starting a server is never the same act as resuming
dispatch: a crash, a `launchd` `KeepAlive` or an infrastructure problem brings
the server back quiet, and `pause` rather than `stop` keeps GitHub intake and
the API alive while it is. An unparseable value is a hard startup error rather
than a silent fallback, because this variable decides whether a machine comes
back dispatching. The stored column is deliberately kept — it is still
load-bearing at runtime, just no longer read at boot — and both the code and
CLAUDE.md say so out loud.

The one deliberate exception is a deliberate upgrade. `tasks reload` snapshots
the running server's mode *before* the drain (the pause `--when-idle` installs
is the tool, not the intent), hands it to the replacement through the child's
**environment** — spawn and `--foreground` exec alike — and then verifies
against the new pid's `/status` that it came up in it, printing the `curl` that
fixes it if not. It is not a `POST /mode` after boot for three independent
reasons: a POST leaves a window in which the new server runs in its configured
default (with `TASKS_DEFAULT_MODE=play` and a paused old server, that window
*dispatches*), `--foreground` execs so there is no "later" to restore anything
in, and the real environment outranks every `.env`. A cold start, and a
`--force` swap of a server too wedged to answer `/status`, carry nothing —
unknown resolves to quiet, never to dispatching. `reload` resolves
`TASKS_DEFAULT_MODE` as step **0**, before the build and before anything is
signalled, on the same rule as "build first": an unusable value makes `serve`
refuse to boot, and discovering that after the SIGTERM turns a typo into an
outage.

Two problems surfaced on the way and are fixed here. The boot breadcrumb is a
`note` and not `mode_changed`, because `mode_changed` is nudge-worthy, so
emitting the semantically nicer event would have spent one orchestrator agent
turn on every restart, on something the orchestrator has no charter capability
to act on. And `orchestrator_nudge_loop` now checks `*shutdown.borrow()` at the
top of its outer loop, matching every other loop in `run.rs`: `changed()` marks
the value seen when it *returns*, so a shutdown consumed by the inner batch
loop left the outer one waiting for a second change that never came, parking on
`events.recv()` forever while the drain awaited it unbounded. One nudge-worthy
event near a restart was enough to wedge the process until its supervisor's
SIGKILL — `tasks stop` took 75s and each `tasks reload` swap ~75s — and
`POST /mode` is one such event, so "pause the pipeline, then restart it" hit it
every time. Tests cover parsing, the boot overwrite and its non-nudging
breadcrumb, a cold start after a playing server, a configured `play` boot, an
upgrade that carries `play` over, a swap that carries nothing, an unusable
default refused before anything is signalled, and a shutdown mid-burst (which
was verified red without the loop fix). The reload suite also stops spawning a
real agent — nothing set `ORCHESTRATOR_CMD`, so on any machine with `claude`
installed those mode flips started live turns the shutdown then waited out; it
now writes a stub and runs in ~4s instead of 60s+ with timeouts. 528 tests
pass, doctests included; `cargo fmt` and `cargo clippy --workspace
--all-targets` are clean.
