# Session transcripts: what the app needs to render scout chat/history

Written from the SwiftUI app's perspective (app/Tasks). The app wants to show
a scout session as a chat: what the agent said, which tools it ran, what came
back, and the final result — live while the scout runs, and replayable after.

None of that data survives today. Three gaps, in pipeline order:

## Gap 1 — the agent doesn't emit a transcript

`scout-supervisor` runs `SCOUT_AGENT_CMD`, defaulting to `claude --print`
(crates/scout-supervisor/src/main.rs, `run_agent`). Plain `--print` emits only
final response text. The fix is the engine's own typed output:

```
claude --print --output-format stream-json --verbose
```

Every stdout line becomes one JSON event: `system/init` (session id, model,
tools), `assistant` (text + `tool_use` blocks), `user` (`tool_result` blocks),
and a terminal `result` (subtype, duration, cost, turn count). This is the
Claude Code typed output the CLAUDE.md rule already points at — the server
consumes it, no home-rolled loop. No supervisor protocol change needed:
`ScoutEvent::Progress { stream, line }` already carries lines; they just
become one-JSON-object-per-line. Bump the default in the agent image /
supervisor env.

## Gap 2 — the server discards Progress events

`drain_scout_events` (crates/tasks/src/scout.rs) matches
`ScoutEvent::Progress { .. }` and drops it. Needed:

- New table, Tasks-owned state (fine to persist — nothing GitHub-owned):

  ```sql
  CREATE TABLE session_log (
      session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
      seq         INTEGER NOT NULL,          -- per-session, monotonic
      stream      TEXT NOT NULL,             -- stdout / stderr
      line        TEXT NOT NULL,             -- raw line; stream-json when stdout
      logged_at   TEXT NOT NULL,
      PRIMARY KEY (session_id, seq)
  );
  ```

- Append on every Progress event during the drain loop. Best-effort matches
  the protocol's stated semantics (lines may drop under load); an insert
  failure should warn, not kill the scout.
- Store raw lines. Don't parse stream-json server-side beyond validation —
  the schema belongs to the engine, and the app is the renderer. stderr lines
  stay as-is (clone/branch noise, agent diagnostics).

## Gap 3 — no read path

Two routes on the existing axum server (crates/tasks/src/server.rs):

- `GET /sessions/{id}/log?since_seq=&limit=` — catch-up read, same shape as
  `/events`. Returns `[{seq, stream, line, logged_at}]`.
- `GET /sessions/{id}/log/stream` — per-session SSE tail. Deliberately NOT
  folded into the global `/events/stream`: a scout emits hundreds of lines
  and every dashboard subscriber would eat them. The global stream keeps
  state-change events only; one `SessionLogStarted`-style event there is
  optional and probably unnecessary (the app already refreshes sessions on
  `session_started`).

The store needs a per-session broadcast (or one broadcast keyed by session id)
so the SSE route can tail inserts without polling SQLite.

## What the app does with it

Already-planned rendering once the above lands:

- Session detail grows a transcript pane: catch-up via `GET .../log`, then
  live-follow the SSE stream while `status == running`.
- stdout lines parse as stream-json: assistant text renders as chat bubbles,
  `tool_use` as collapsed rows (tool name + input summary), `tool_result`
  inline under its tool, `result` as a footer (duration, cost, turns).
  Unparseable stdout and all stderr render as a monospaced log section —
  nothing is hidden.
- No app-side persistence; the server log is the source of truth, same as
  everything else.

## Sizing / retention notes

- A scout run is typically a few hundred to a few thousand lines; tool
  results dominate. At ~1–2 MB/session worst case, SQLite is fine. If it ever
  matters, cap per-session rows or add a retention sweep keyed on
  `sessions.completed_at` — decide when it hurts, not now.
- `ScoutEvent::Progress` is lossy by design; per-session `seq` is assigned
  server-side at insert, so gaps in agent output don't break ordering.
