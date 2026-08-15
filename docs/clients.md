# Writing a Tasks client

How a UI (gpui app, TUI, CLI, Claude Code session) should talk to the
Tasks server. The HTTP API on `127.0.0.1:4800` (`TASKS_SERVER_PORT`) is the
only interface — clients never touch SQLite, GitHub, or vm-pool directly.

## The idiomatic client loop

Tasks is event-sourced at the edge: every write the server performs appends
an `Event` with a monotonically increasing `seq`, and the API exposes both a
catch-up read (`GET /events?since=`) and a live stream (`GET /events/stream`,
SSE with 15s keepalive comments). The intended client shape:

1. **Open the SSE stream first**, then snapshot (`GET /tasks`, `/sessions`,
   `/specs`, `/spec-queue`, `/mode`, `/projects`). Opening the stream before
   snapshotting means no gap between the two.
2. **Treat events as invalidation signals, not data.** Payloads are
   deliberately identifier-only (`task_id`, `spec_id`, …). On receiving an
   event, refetch the affected entity (`GET /tasks/{id}`) or the affected
   list. Don't reconstruct entity state by folding event payloads client-side
   — the server's row is the truth, the event just tells you it changed.
3. **Track the last `seq` you've seen.** On reconnect (or SSE lag — the
   broadcast buffer holds 1024 events and drops the oldest for slow
   consumers), resync with `GET /events?since=<last_seq>` and then resume the
   stream. If the gap is large, just re-snapshot; it's cheap.
4. **Writes are plain POSTs and return the updated resource(s)**, so you can
   apply the response optimistically; the corresponding event will also
   arrive on the stream (dedupe by refetching, which is idempotent).

No auth, loopback only — don't build a login flow.

## Interaction surface (where to hook UI actions)

| UI action | Call | Semantics |
| --- | --- | --- |
| Add a repo | `POST /projects` `{"repo_owner","repo_name"}` | 201 with the project; 400 if it already exists |
| Reorder task queue (drag & drop) | `POST /queue/reorder` `{"task_ids":[...]}` | **Full order, front to back.** Ranks are rewritten 1..N transactionally; any task not listed becomes unranked and sorts after all ranked tasks (then priority desc, then ingested_at). Send the complete on-screen order after every drop. The response is the same projection as the default `GET /tasks` (closed intake hidden), so it can replace the client's list directly. |
| Reorder spec queue | `POST /spec-queue/reorder` `{"spec_ids":[...]}` | Same full-order semantics |
| Pick up a task | `POST /tasks/{task_id}/queue` | `backlog` → `queued`, appended at the end of the ranked order. 400 unless the task is `backlog`. The only door from the backlog into the pipeline — scouts dispatch **only** queued tasks. |
| Un-pick a task | `POST /tasks/{task_id}/dequeue` | `queued` → `backlog`, rank cleared. 400 once work has started (past `queued`). |
| Scout now | `POST /tasks/{task_id}/scout` | Queue the task (from `backlog` or `queued`) at the **front**, shifting everything else down. The dispatch loop picks it up on its next tick; the concurrency cap still applies — it jumps the queue, it doesn't bypass it. |
| Review a spec | `POST /spec-queue/{spec_id}/review` `{"status","feedback"?}` | `status` ∈ `approved` \| `needs_revision` \| `rejected`. `approved` → task `ready_to_build`; `needs_revision` → task returns to `queued` for a re-scout (feedback reaches the next scout's prompt); `rejected` → dead end. |
| Play / pause / stop | `POST /mode` `{"mode":"play"\|"pause"\|"stop"}` | Gates **new** work only. A mode change never interrupts a scout in flight — reflect that in the UI (pausing ≠ cancelling; show in-flight sessions still running). |

Everything else is read-only. There is deliberately no
`POST /tasks` (tasks come from GitHub issue intake), no task-edit endpoint,
and no way for a client to write GitHub state — GitHub writes funnel through
the server only.

## Reads and their shapes

- `GET /tasks` — already in queue order; render it as-is, don't re-sort.
  Task: `{id, project_id, gh_issue_number, title, body, labels, gh_state,
  state, priority, manual_rank, dispatch_attempts, ingested_at, updated_at}`.
  By default the list **omits tasks whose issue is closed on GitHub and whose
  state is `backlog`, `done`, or `rejected`** — closed-before-any-work intake
  noise, and work already concluded (closure-derived retirement's output).
  In-flight work stays visible whatever its `gh_state` (the poller retires it
  properly), and a `done`/`rejected` task whose issue is still *open* also
  shows — that's the "close the issue or re-queue?" decision surface.
  `GET /tasks?all=true` returns every row. Ordering is identical either way.
- `GET /sessions` / `GET /sessions/{id}` — scout runs.
- `GET /specs` / `GET /specs/{id}` — `spec_markdown` is the deliverable;
  render it as Markdown. Also carries `files_touched`, `complexity`,
  `agent_exit_code`.
- `GET /spec-queue` — `{entry: {...}, task_id}` items for the review screen.
- `POST /builds` `{"spec_ids": [...], "base_branch"?}` — queue a Builder run
  over a set of **approved** specs; **202** with the build (`{..., spec_ids}`).
  Builds are strictly serial — one runs at a time, each cut from a base that
  already contains the previous one — so 202 means queued, not started. The
  batch is re-sorted into spec-queue order server-side. 400 on an empty or
  duplicated set, a non-approved spec, specs from two projects, or a spec
  already in an active build. Watch `build_*` events or poll.
- `POST /orchestrator/messages` `{"content"}` — say something to the
  orchestrator (a persistent server-side Claude Code session that inspects
  and drives the pipeline over this same API). **202** with your message; the
  reply arrives asynchronously as an `orchestrator_message` event — refetch
  on it. `GET /orchestrator/messages?since=N` returns turns with `seq > N`,
  oldest first: `{seq, role: "user"|"assistant"|"event", content,
  created_at}`. `event` turns are automated pipeline notifications the server
  injects (specs landing, builds finishing, tasks ingested — debounced into
  one turn per burst); the orchestrator answers them proactively like user
  turns. Render them as compact system lines, not chat bubbles, and render an
  "answering…" affordance while the newest turn is a `user` or `event` turn.
  Tolerate unknown roles.
- `GET /orchestrator/session` — `{cc_session_id, workdir, checked_out}`: the
  orchestrator's Claude Code session, resumable interactively with
  `cd <workdir> && claude --resume <cc_session_id>`. `cc_session_id` is null
  until the first tick. `POST /orchestrator/session/checkout` (409 while
  there's no session) marks it interactively held — headless ticks suspend,
  nudges queue as unanswered turns — and must be re-POSTed at least every 5
  minutes (it's a heartbeat; a dead client lapses on its own). `POST
  /orchestrator/session/release` ends the hold; queued input is answered on
  the next tick. Wrap interactive use: checkout + renew loop, run `claude
  --resume`, release on exit.
- `GET /orchestrator/stream` — SSE live view of the in-flight tick, one JSON
  frame per `data:` line: `{"kind":"started"}` (a tick began — published
  before the agent is even spawned), `{"kind":"delta","text"}` (assistant
  text in generation order), `{"kind":"tool","label"}` (a tool call, e.g.
  `Bash: curl …`), `{"kind":"done"}` (the durable reply is now fetchable).
  **Ephemeral**: no backfill, a (re)connect only sees what happens next, and
  a lagged subscriber misses deltas — never the reply, which always lands in
  `/orchestrator/messages`. Suggested rendering: accumulate deltas into a
  growing bubble, reset the accumulator on each `tool` frame (pre-tool text
  is working narration; the segment after the last tool call is the reply),
  show the latest tool label as status, and swap in the persisted message
  when it arrives. Skip unknown `kind`s.
  Drive the "is it working" indicator off the tick's **lifecycle** — an
  elapsed clock from `started` (or from your own send) until the reply lands
  — not off text arriving: extended thinking and slow tool calls are long
  silences, and an operator who overrides `ORCHESTRATOR_CMD` without
  `--output-format stream-json --verbose --include-partial-messages` gets
  `started`/`done` and nothing in between. Retire the provisional view when
  the durable reply arrives rather than on `done`: `done` can be dropped
  (lagged subscriber, dropped connection, a server that died mid-tick), and
  a view retired only by `done` leaves a clock running forever when it is.
- `GET /builds` (newest first), `GET /builds/{id}` (`{..., spec_ids}`) —
  `branch`, `base_branch`, `base_sha`/`head_sha`, `pr_number`, `status`,
  `summary` (the PR body the agent wrote), `files_touched`, `exit_reason`.
  `pr_number` is an identifier, not a state: the PR's mergeability/CI/open
  state is GitHub's — link out (`https://github.com/{owner}/{repo}/pull/N`),
  don't expect it here.
- `GET /briefings` — the three Home briefing slots, always all three, in
  display order: `[{section, content, generated_at, stale, regenerating,
  error}]` with `section` one of `state_of_project` | `changes` | `issues`.
  `content` is LLM-written markdown prose (null until first generation);
  render it with the generated_at age visible — the prose is a **cache with
  a date**, and prose without its date reads as current. **Reading is the
  demand signal**: the server returns stored copies immediately and kicks a
  single-flight background regeneration for stale sections
  (stale-while-revalidate, TTL `BRIEFING_TTL_SECS`, default 900s); a
  completed regeneration appends a `briefing_updated` event — refetch on it.
  `regenerating` means a run is in flight ("refreshing…"); `error` carries
  the last failed attempt while the previous good copy keeps serving
  ("couldn't refresh"). Don't poll on a timer — the SSE loop's
  refetch-on-event plus fetching when the surface is visible is the whole
  contract.
- `GET /events?since=&limit=` — catch-up (default limit 100);
  `GET /events/stream` — SSE, each `data:` line is one Event JSON.

## Counting the event log

Dashboards that answer "what happened this week" must count the **event log**,
not the entity lists, and must hold the **whole** log:

- **Don't count from `/tasks`.** The working set reconciles away work whose
  issue closed — which is exactly the shipped work a dashboard wants to count.
  (`GET /tasks/{id}` still serves retired tasks; use it to name old work.)
- **Don't trust `updated_at`** as an activity timestamp — any poll can touch it.
- **The newest-N trap:** `GET /events` without `since` returns the newest
  `limit` events. A fold over that page silently undercounts the moment
  in-window events scroll off it — a fabricated quiet week, not an error.
  Instead, backfill once by paging `?since=1&limit=500` (advance
  `since = high_water + 1`; `since` is inclusive, so *also* filter the page on
  `seq > high_water` — two interleaved syncs must not double-count), then
  extend with one delta request per refresh.
- **The counted kinds** are `task_ingested`, `spec_created`,
  `spec_queue_status_changed` (an approval is `to == "approved"`; a rejection
  is the **same kind** with a different `to`), `build_completed` (failures
  included — it counts trips through the pipeline), and `pull_request_opened`.
- **Nothing on the wire counts merged pull requests.** `pull_request_opened`
  fires at open; merged/closed state is GitHub's, queried at render time or
  not shown. Don't present opened PRs as shipped work.

## Session transcripts

Agent output is a **separate channel from the event log**, on purpose (see
*Event volume* below). Two endpoints, both scoped to one session:

- `GET /sessions/{id}/transcript?since=<seq>&limit=` — catch-up read. Returns
  `[{session_id, seq, timestamp, stream, line}]`, oldest first. `stream` is
  `stdout` \| `stderr`. `since` is **inclusive**, matching `/events?since=` — a
  tailing client passes `last_seq + 1`. `limit` defaults to 500, capped at 2000.
- `GET /sessions/{id}/transcript/stream` — SSE tail. Replays from `since` (all
  of it, paging internally — no `limit`, so the stream can't silently skip a
  span) and then streams live lines, with the same 15s keepalive as
  `/events/stream`.

`seq` is dense per session and assigned server-side at persist time. An empty
transcript means *nothing was recorded* — sessions predating transcript capture
have none — so render it as "no transcript", never as a failure.

Lines are the agent's stream-json output: one JSON object per line (assistant
messages, thinking, tool calls with their inputs, tool results, and a final
`result` record). Treat each `line` as opaque text unless you're prepared to
parse Claude Code's schema — it's forwarded verbatim, and a transcript can
contain any file the agent read, so it carries the same trust boundary as the
rest of this API: local SQLite, loopback only, no auth.

`Session` also carries `usage` (`{input_tokens, output_tokens,
cache_read_input_tokens, cache_creation_input_tokens, total_cost_usd,
duration_ms, num_turns}`), parsed from that final record. Every field is
nullable and so is `usage` itself — a renamed upstream key costs a null, not a
failed scout.

Two caps bound a session server-side: 32 KiB per line (over-long lines are cut
and marked) and 8 MiB per session, after which one notice is written and
recording stops — the scout itself is unaffected. Lines lost to queue pressure
are announced inline as `[tasks] N transcript line(s) dropped here` rather than
left as an invisible gap.

## Event volume

The event log is deliberately low-rate. Clients are told to treat events as
invalidation signals and refetch, so every event costs every connected client a
request — which only works while events are rare and meaningful (a state
change, a spec, a mode flip). High-rate data must not go through it.

Transcripts are the worked example: a single scout emits thousands of output
lines, and only an open session-detail view wants them. They live in their own
table, their own broadcast channel and their own endpoints, and they never
touch `append_event`. Apply the same rule to anything similar you add later.

All enums are snake_case strings on the wire:

- `Task.state`: `backlog` (ingested, inert — the Tasks table) → `queued`
  (explicitly picked up — the Queue, ordered by `manual_rank`) → `scouting` →
  `in_review` (spec awaits a verdict) → `ready_to_build` (approved, parked
  for a Builder) → `building` (a Builder run is implementing it, possibly
  batched with others) → `done`, with `rejected` as the terminal failure
  state. A failed build returns `building → ready_to_build` — the spec is
  still good.
  Scout failures and `needs_revision` verdicts return to `queued`, never
  `backlog` — picked-up work stays picked up.
- There is no "mark done" endpoint, deliberately: **closing the GitHub issue
  is the done signal.** When a picked-up task's issue closes, the next poll
  retires it — `queued`/`in_review`/`ready_to_build` become `done` (or
  `rejected` if the issue was closed as not-planned / duplicate), the task's
  `manual_rank` clears, and a normal `task_state_changed` event fires. A
  `scouting` task is left to finish first and retires from `in_review` on the
  following poll. Clients shouldn't offer a complete/done action; link to the
  issue instead.
- `Session.status`: `running`, `scout_succeeded`, `scout_failed`, `cancelled`.
- `SpecQueueEntry.status`: `pending_review`, `approved`, `needs_revision`,
  `blocked`, `rejected`, `built` (only the three verdict values are accepted
  by the review endpoint; `pending_review`/`blocked` are server-assigned, and
  `built` is how the approved queue drains — a successful Builder run
  assigns it, and a spec cannot be built twice).
- `Build.status`: `queued` → `running` → `succeeded` | `failed`.
- `Complexity`: `simple`, `medium`, `complex`.
- `Mode`: `play`, `pause`, `stop`.

Parse enums leniently (unknown value → show raw string, don't crash): new
states will appear as the pipeline grows.

Event JSON: `{seq, timestamp, payload}` where payload is tagged by `"kind"`
(snake_case): `project_added`, `task_ingested`, `task_state_changed`
(`from`/`to`), `task_gh_state_changed` (`task_id`, `gh_state` — the poller's
snapshot of GitHub's open/closed flag moved, most often because the issue
dropped out of the repository's open set; refetch the task or the list),
`session_started`, `session_completed`, `spec_created`,
`spec_queue_status_changed`, `queue_reordered`, `spec_queue_reordered`,
`build_requested` (`build_id`, `spec_ids`), `build_started`,
`build_completed` (`build_id`, `status` — refetch the build for detail),
`pull_request_opened` (`build_id`, `pr_number`),
`orchestrator_message` (`seq`, `role` — refetch `/orchestrator/messages`),
`mode_changed`, `note` (`source`, `message` — free-form breadcrumbs; a
scrolling activity feed of these is the cheapest useful "what is it doing"
view).

## Design constraints worth mirroring in the UI

- **GitHub-owned facts are queried, not stored** (server-side rule). For the
  client this means `gh_state` etc. are snapshots from the last poll — label
  them as such, don't present them as live.
- **`manual_rank` is human-authoritative.** The reorder endpoints are the
  only writers. The GitHub poller can change `priority` but never ordering —
  so a user's drag & drop is never clobbered by a poll.
- **The spec is the deliverable** (Scout/Builder information barrier). The
  review screen is the centerpiece of the whole product: spec markdown,
  verdict buttons, feedback field. Scout code is intentionally not available
  to show.
- **Sessions can be long.** The first live scout took 23 minutes. Show
  elapsed time, not a spinner that looks hung.

## Changes in flight / coming (PR #757 and roadmap)

- **Landed in PR #757** (branch `reconcile-and-attempts`): `Task` gains
  `dispatch_attempts`; three consecutive failed dispatches move a task to
  `rejected` with a dispatcher `note` explaining why; on server startup,
  orphaned `running` sessions become `scout_failed`
  (`exit_reason: "orphaned by server restart"`) and stuck `scouting` tasks
  return to `queued` — clients just see the normal events.
- **Re-scout feedback loop** (next branch): `needs_revision` feedback will be
  fed into the next scout's prompt. No API change expected; the review call
  you make today is already the right hook.
- **Progress visibility** (planned, coordinate before building): per-session
  scout output (agent stdout/stderr lines) buffered server-side and exposed
  via the API/SSE — the hook for a live session-detail view. Not built yet;
  today `Progress` lines only reach the server logs.
- **Builder / Diamond 2** (later): a builder-run resource referencing a *set*
  of spec ids (batching is core to the design — model the review queue with
  multi-select in mind), plus PR links as the builder's output.
- **Orchestrator** (#756): spec reviews will increasingly be posted by an
  agent through this same API; the UI's review verdicts and the
  orchestrator's are the same call, so nothing special to do — but expect
  `spec_queue_status_changed` events you didn't initiate.
