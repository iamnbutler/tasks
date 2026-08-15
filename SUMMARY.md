Three fixes to numbers and mechanisms that were quietly lying about
themselves. **#827:** `Orchestrator::context_tokens` summed the input side of
the stream-json `result` record, which aggregates usage across every internal
turn of one `claude --print` invocation — each re-reading the cached prefix.
That is cost-per-tick (2.7M on a live server), not context size, and the doc
comment asserted the opposite. Context size now comes from the last main-chain
`assistant` record's `message.usage` — the prompt behind a single model call,
and therefore a genuine absolute reading — while the `result` aggregate keeps
being recorded under the name that describes it, `tick_tokens`. Sub-agent
turns (non-null `parent_tool_use_id`) are excluded from the gauge but still
reach the live feed. Migration `0018` renames the old column to
`last_tick_tokens` so its values keep their true meaning; `last_context_tokens`
starts NULL rather than reinterpreting numbers that were never a context size,
so `GET /orchestrator/session` reports a blank gauge until the next tick. Item
7 of the orchestrator-mind plan can now build a rotation threshold on it and
get what it asked for.

**#826:** the orchestrator's authority is enforced by attribution — the charter
gates `Actor::Orchestrator` and never `Actor::Human` — so a write the server
cannot attribute silently acquires full human authority. The old mechanism, a
`TASKS_ACTOR_TOKEN` interpolated into `-H "X-Tasks-Actor: orchestrator $..."`,
could not be run under the default `--allowedTools Bash(curl:*)`, because
Claude Code will not statically verify a Bash command containing a variable;
the safest deployment was therefore the one where the charter was inert. The
token now reaches the agent as a server-written curl config file
(`<data dir>/orchestrator-curl.conf`, mode 0600, written atomically before
every spawn, holding only the header line), and the prompt tells it to pass
`-K <path>` — a static command that matches the allowlist and keeps the token
out of argv. The env var is removed rather than kept as a fallback, a turn that
cannot write the file fails rather than running unidentified, and an
`X-Tasks-Actor` header that is present but does not verify is now a 403 instead
of being read as the human. **#824:** `build_5c65e18a` hit its 3600s budget on
schedule and then spent 84 minutes inside `deallocate`, holding the serial
build queue and writing nothing to the event log. The dispatchers had bounded
only the drain, leaving a round-trip to vm-pool unbounded on every path
including failure; both now use a shared `deallocate_bounded` with its own
120s budget, where abandoning teardown is an event-log note rather than
silence. And because `completed_at` is stamped by the finalizers — after
teardown, and on success after the push and the PR — a new
`builds.agent_finished_at` (migration `0017`) records the moment the drain
ended, so the interval the run budget actually bounds is the one clients can
render as "took".

All three land as one change: `make test-cargo` (`CARGO_BUILD_JOBS=1 cargo test
--workspace`) is fully green including doctests, and `cargo clippy --workspace
--all-targets` and `cargo fmt --all` are clean. Wire types gain fields
(`OrchestratorSessionInfo.tick_tokens`, `OrchestratorSession.last_tick_tokens`,
`Build.agent_finished_at`); per the repo's stated position that clients ship
from here, no compatibility shims were added — `app-gpui` only deserializes
these types and needs no edits, though a build-detail view showing "took"
should read `agent_finished_at - started_at`.
