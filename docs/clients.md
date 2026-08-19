# Writing a Tasks client

How a UI (gpui app, TUI, CLI, Claude Code session) should talk to the
Tasks server. The HTTP API on `127.0.0.1:4800` (`TASKS_SERVER_PORT`) is the
only interface — clients never touch SQLite, GitHub, or vm-pool directly.

## Check the build on connect

Both ends ship from one tree, so the wire types are shared and skew is
normally a build error. What that doesn't cover is two *processes* from
different commits: a server rebuilt and restarted under an app that wasn't.
The symptom is a pile of unrelated failures — a strict enum that doesn't know
a variant, a field that isn't there — which reads as "the server is broken"
when the fact is "your app is old".

`GET /version` is the answer. Unauthenticated, store-free and first in the
router, so it also answers while the rest of the process is still starting:

```json
{ "version": "0.1.163", "commit": "1a7b6c8", "min_client_version": "0.1.0" }
```

`version` is `0.1.<commit count>`; `commit` is the short SHA (`-dirty` for an
uncommitted tree). Both are `unknown`/the crate version if the binary was
built without git in reach — that itself tells you it wasn't a `make` install.
Compare versions **numerically, component-wise**: `0.1.100` is newer than
`0.1.9`, and string comparison gets that backwards.

`min_client_version` is the oldest client the server expects to speak to. It
moves by hand and only for an actual wire break — it deliberately doesn't
follow `version`, since a floor equal to the current build declares every
client stale and conveys nothing.

**Under-minimum clients are warned, never refused.** Every route keeps
answering. In a single-user system the value here is the diagnosis; a 426 on
every route would turn one legible sentence back into the wall of failed
requests this exists to replace.

From `tasks-client`, that's one call on every `Connected`:

```rust
let client = Client::from_env().with_client_version(about::VERSION);
// ... on each EventStreamItem::Connected, from a worker thread:
if let Some(warning) = client.preflight().ok().and_then(|v| v.warning()) {
    // "This client build (0.1.120) is older than the server supports
    //  (needs 0.1.140, server is 0.1.163) — rebuild the client (`make app`)."
}
```

Check it on *every* connect, not just the first: a reconnect is usually a
server that restarted into a new build. `warning()` is `None` when there is
nothing certain to say — including when either build is unidentifiable, since
a warning that fires on merely unlabelled builds gets trained out of use.

A **404** from `/version` is a verdict rather than an error: that server
predates the route, so it is the stale one (`Preflight::ServerUnversioned`).
Only a transport failure is an `Err`, and that is the "can't reach the server"
case a client already handles. There is no `min_server_version` and no warning
for a client merely *newer* than the server — the 404 is today's only
reverse-direction signal, though `Preflight::server()` vs `client_version()`
has the data if you want it.

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

### What a client must not send

A bind address is not access control against a browser: with no auth, a
request carrying no `X-Tasks-Actor` is the human's, and the human is never
gated. So every route — reads included — goes through one guard
(`crate::loopback`), and there are two ways to trip it:

- **The authority must be this machine's loopback.** `127.0.0.0/8`, `::1`
  (bare or bracketed) or `localhost`, with or without a port; the port is
  parsed but never compared, so `:0` in a test is as good as `:4800`. Every
  authority the request states is checked — the `Host` header (all of them, if
  something sent more than one) *and* the URI authority, which is where HTTP/2
  states it. Anything else is **403**. This is what a DNS rebind fails on: a
  browser fills `Host` from the URL's hostname, so an attacker's name that
  resolves to `127.0.0.1` still arrives as `evil.example:4800`.
- **An `Origin` header is a refusal — any value at all**, including `null` and
  including a loopback one. This API has no browser clients, so the header's
  presence is the finding; a future local web UI gets an explicit allow-list
  on the day it exists, decided by someone thinking about it.

No client in this tree has to do anything: `ureq` (the app, `tasks status`,
`tasks-client`) and `reqwest` (`tasks reload`) both derive `Host` from the URL
and send no `Origin`, and the orchestrator's `curl -K` config holds one line —
the actor header. If you are writing a new client, just don't add an `Origin`
and don't rewrite the `Host`.

**Deployment, not client library, is what this can break.** Through an SSH
`-L` tunnel the client connects to `localhost:PORT`, so the `Host` arrives
loopback and nothing changes. Through an **HTTP reverse proxy** (`tailscale
serve`, nginx) the proxy forwards its own `Host` and every request 403s — that
is accepted breakage, and the tunnel is the answer. There is deliberately no
switch to turn the guard off, and no `X-Forwarded-Host` allowance: that would
trust a header any client can send on a listener with no way to tell a proxy
from a web page.

**What the guard does not cover**, stated so it doesn't become nobody's job: a
cross-site subresource `GET` straight to loopback. Browsers send no `Origin`
on `<img src>`, `<script src>` or `<iframe>`, so such a request passes both
rules. Responses stay unreadable to the attacker (this API sends no CORS
headers), so the residual is routes with a server-side effect — today just
`GET /decisions/{seq}/reconcile`, which spends the server's own GitHub
credential outbound. That one is accepted: idempotent, locally inert, and only
ever for a decision still `pending`.

## Interaction surface (where to hook UI actions)

| UI action | Call | Semantics |
| --- | --- | --- |
| Add a repo | `POST /projects` `{"repo_owner","repo_name"}` | 201 with the project, born `active`; 400 if the repo is already tracked — matched **case-insensitively**, so `Owner/Repo` will not become a second project for `owner/repo`. **Human-only**: any other actor is a 403, on the same footing as Build Now — adding a repo commits VM hours and authorises pull requests against somebody's repository, and no charter capability covers that. Nothing is dispatched by it: ingested issues land in `backlog`, and backlog never dispatches. |
| Pause / archive a repo | `POST /projects/{project_id}/status` `{"status":"active"\|"paused"\|"archived"}` | How much of the pipeline runs for one repo — see the table below. Gates **new** work only, exactly like the mode: nothing here interrupts a scout or a build in flight. **Human-only** (403 otherwise), and **there is no delete** — `decisions` is append-only and keyed to a project's tasks, and `tasks.project_id` is `ON DELETE CASCADE`, so archive *is* the removal. An unknown word is a 400 naming the three legal ones. |
| Reorder task queue (drag & drop) | `POST /queue/reorder` `{"task_ids":[...]}` | **Full order, front to back.** Ranks are rewritten 1..N transactionally; any task not listed becomes unranked and sorts after all ranked tasks (then priority desc, then ingested_at). Send the complete on-screen order after every drop. The response is the same projection as the default `GET /tasks` (closed intake hidden), so it can replace the client's list directly. |
| Reorder spec queue | `POST /spec-queue/reorder` `{"spec_ids":[...]}` | Same full-order semantics |
| Pick up a task | `POST /tasks/{task_id}/queue` | `backlog` → `queued`, appended at the end of the ranked order. 400 unless the task is `backlog`. The only door from the backlog into the pipeline — scouts dispatch **only** queued tasks. |
| Un-pick a task | `POST /tasks/{task_id}/dequeue` | `queued` → `backlog`, rank cleared. 400 once work has started (past `queued`). |
| Scout now | `POST /tasks/{task_id}/scout` | Queue the task (from `backlog` or `queued`) at the **front**, shifting everything else down. The dispatch loop picks it up on its next tick; the concurrency cap still applies — it jumps the queue, it doesn't bypass it. |
| Build now | `POST /tasks/{task_id}/build-now` `{"content"?,"complexity"?,"base_branch"?,"rationale"?}` | Skip the Scout for a task whose issue body already *is* the spec. Writes the spec by hand, approves it, and queues a Builder run over it in one call; **202** with the same `{..., spec_ids}` shape as `POST /builds`. Legal only from `backlog` or `queued` — the states where no Scout has run and none is running (`backlog` → `ready_to_build` is a new edge). Every field is optional: the spec defaults to the issue body, complexity to `simple`. A supplied `content` **replaces** the body rather than extending it. 400 if that leaves nothing to build from. **Human-only**: any other actor is a 403, because this authors, approves and dispatches with no second opinion anywhere in the loop — no charter capability covers that. |
| Cancel a run | `POST /sessions/{session_id}/cancel` \| `POST /builds/{build_id}/cancel` `{"rationale"?}` | Stop a scout or a build that is already in flight. 404 for a run that does not exist, 409 for one that has already concluded. The run ends as **`cancelled`** — never `failed` — with the actor and rationale in `exit_reason`, and the work comes back: a scout's task to **`backlog`** (not `queued`, or the dispatch loop would start a replacement within the tick) and a build's specs to `approved` with no attempt charged. The ack is `{run_kind, run_id, concluded, status, decision_seq, note}`; **`concluded: false` is not a failure** — the request is recorded and the dispatcher following the run concludes it, so watch for `session_completed` / `build_completed` rather than retrying. A queued build is cancelled outright (nothing is following it). Cancelling a run that finishes in the same breath leaves the run's real outcome standing and says so. |
| Delete a preserved bundle | `DELETE /builds/{build_id}/bundle` | Throw away the only copy of an implementation whose branch never landed. **204**; 404 when there is nothing there (a second click, or the retention policy got there first); 503 without a bundle directory. **Human-only** — the orchestrator is a 403, on the same footing as Build Now: there is no undo, and anything the retention policy has not already reclaimed is by construction work nobody reproduced. Arm it behind a second click and say what is being lost. |
| Review a spec | `POST /spec-queue/{spec_id}/review` `{"status","feedback"?}` | `status` ∈ `approved` \| `needs_revision` \| `rejected`. `approved` → task `ready_to_build`; `needs_revision` → task returns to `queued` for a re-scout (feedback reaches the next scout's prompt); `rejected` → dead end. |
| Play / pause / stop | `POST /mode` `{"mode":"play"\|"pause"\|"stop"}` | Gates **new** work only. A mode change never interrupts a scout in flight — reflect that in the UI (pausing ≠ cancelling; show in-flight sessions still running — cancelling one is `POST /sessions/{id}/cancel`, above). The mode is **not** remembered across restarts: every boot starts in the server's configured `TASKS_DEFAULT_MODE` (default `pause`), and only `tasks reload` carries the old mode to its replacement. So a client that survives a server restart must re-read `/mode` (or `/status`) rather than trusting its last snapshot — reconnecting to the event stream and resnapshotting, which a restart forces anyway, already does this. |

Everything else is read-only. There is deliberately no
`POST /tasks` (tasks come from GitHub issue intake), no task-edit endpoint,
and no way for a client to write GitHub state — GitHub writes funnel through
the server only.

### Project status

| `status` | issues ingested | scouts / builds dispatched | in the client's lists |
| --- | --- | --- | --- |
| `active` | yes | yes | yes |
| `paused` | yes | no | yes, badged |
| `archived` | no | no | listed last |

`archived` stops the *upsert* half of the poll and nothing else: the repo is
still fetched, closures are still reconciled, and an open Builder PR is still
watched. That costs one GitHub fetch per archived project per poll, forever,
and it is the right price — closure is only ever learned from *absence* in the
open set, so a repo that stopped being fetched would leave every task it
already has stuck at `gh_state = open`.

**Mode is global — don't render project status as a second play/pause.** Mode
gates a dispatcher whose real constraints are server-wide (one
`SCOUT_MAX_CONCURRENT`, one strictly serial build lane, one vm-pool), so a
per-repo `play` could not run while another repo's build held the lane. What is
honestly per-repo is the *subtraction*: a project the dispatcher skips.

`GET /projects` returns archived projects too, deliberately: a task whose
project is archived still carries that `project_id`, and a filtered list would
leave the row with no repo to name. Sort them last rather than dropping them,
or a repo you cannot select becomes a repo you cannot un-archive.

Every body that writes to GitHub (`POST /issues`, the comment/edit/label/merge
routes) carries an optional `project_id`. It is only optional when exactly one
**non-archived** project exists; otherwise the server refuses to guess rather
than filing into the wrong repo. Name it explicitly in a multi-repo client.

## Reads and their shapes

- `GET /tasks` — already in queue order; render it as-is, don't re-sort.
  Task: `{id, project_id, gh_issue_number, title, body, labels, gh_state,
  state, priority, manual_rank, dispatch_attempts, ingested_at, updated_at}`.
  Ordering is **terminal states last** (`done`, `rejected`), then
  `manual_rank` (nulls last), then priority descending, then oldest first.
  Only the terminal group is pulled out: sorting the whole pipeline by state
  would override `manual_rank`, which is the human's statement of what to do
  next.
  By default the list **omits tasks whose issue is closed on GitHub and whose
  state is `backlog`, `done`, or `rejected`** — closed-before-any-work intake
  noise, and work already concluded (closure-derived retirement's output).
  In-flight work stays visible whatever its `gh_state` (the poller retires it
  properly) — including `awaiting_merge`, where the merge is the very thing
  that closed the issue — and a `done`/`rejected` task whose issue is still
  *open* also shows, that's the "close the issue or re-queue?" decision
  surface. `GET /tasks?all=true` returns every row. Ordering is identical
  either way.
- `GET /sessions` / `GET /sessions/{id}` — scout runs. `status` has two more
  terminal values besides `scout_succeeded` / `scout_failed`:
  **`scout_stopped_early`** — the run ended without a spec but left notes
  behind (see *Scout notes* below); treat it as neither success nor failure —
  and **`cancelled`**, which somebody asked for. A cancelled run's
  `exit_reason` reads `cancelled by <actor>: <rationale>`, which is the only
  thing distinguishing it from a crash; it keeps whatever notes it had
  checkpointed, and it costs the task no dispatch attempt.
- `GET /sessions/{id}/notes` — salvage from a run that stopped early;
  **404 when there is none**, which is the ordinary case.
- `GET /bundles` / `GET /builds/{id}/bundle` — implementations whose branch
  could not be pushed; see *Preserved bundles* below. Newest first, an empty
  list is ordinary, and the per-build read is **404 when there is none** (the
  `/sessions/{id}/notes` shape). A server with no bundle directory answers
  **503 rather than `[]`** — "nothing was preserved" is the one wrong answer to
  give about work that exists in exactly one place, so treat 503 as "cannot
  say" and show nothing rather than "none".
- `GET /specs` / `GET /specs/{id}` — `spec_markdown` is the deliverable;
  render it as Markdown. Also carries `files_touched`, `complexity`,
  `agent_exit_code`. **`session_id` is nullable**: null means no Scout ran and
  a human wrote this spec by hand (`POST /tasks/{id}/build-now`). Such a spec
  has no transcript, no session to link to, and no independent reviewer — the
  human who wrote it is the review. Say "human-authored" where the scout link
  would go rather than leaving the absence to be inferred, and keep
  `files_touched` empty as the server does: it is genuinely unknown, and an
  invented one feeds the overlap briefs a lie rather than an omission.
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
  `/orchestrator/messages`. Skip unknown `kind`s.
  Two renderings are legitimate, and they share one rule: **the segment after
  the last `tool` frame is the reply**, so drop it when the durable message
  lands rather than printing the answer twice. *Minimal rendering* — one
  growing bubble; reset the accumulator on each `tool` frame (pre-tool text
  is working narration), show the latest tool label as status, swap in the
  persisted message when it arrives. *Stacked rendering* — keep a flat row
  list in which durable turns and the tick's text segments and tool groups
  interleave in arrival order, coalescing consecutive `tool` frames into one
  expandable group, so a turn that went text → tool → text reads down the
  page instead of overwriting itself. A turn that arrives mid-tick belongs
  *above* the open trail. Concluded trails are **session-local scrollback**:
  keep them for the life of the client process, but let a restart or
  reconnect collapse the conversation back to the durable
  `/orchestrator/messages` — persisting a feed the server documents as
  ephemeral would be the wrong side of that contract.
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
  `status` ∈ `queued` \| `running` \| `succeeded` \| `failed` \| **`cancelled`**
  — the last of those is somebody stopping the run, not the build failing, and
  its batch's specs stay `approved` with no attempt charged.
  A build has **two durations**, and they are not interchangeable:
  `agent_finished_at - started_at` is the agent phase — what the run budget
  bounds, and what to render as "took" — while `completed_at` also includes
  VM teardown and, on success, the branch push and the PR. When they differ
  by a lot, the gap is infrastructure, not the agent.
  `pr_number` is an identifier, not a state: the PR's mergeability/CI/open
  state is GitHub's — link out (`https://github.com/{owner}/{repo}/pull/N`),
  don't expect it here.
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

## Scout notes — salvage, never a spec

A scout that dies before writing `SPEC.md` used to lose its whole run. It now
keeps a `NOTES.md` as it works, the supervisor streams it back every 30s, and
whatever arrived is persisted. A run that ends without a spec but with notes
gets `status: "scout_stopped_early"`, and its notes are readable at
`GET /sessions/{id}/notes`:

```json
{"session_id": "...", "task_id": "...", "reason": "scout timed out after 3600s",
 "notes": "# Salvage from an interrupted scout run\n…", "files_touched": [],
 "updated_at": "..."}
```

**These notes are not a spec and must never be rendered as one.** They are
unverified exploration: no `Spec` row exists, no queue entry, no review path,
and no verdict was ever passed on them. Their one consumer inside the system is
the next attempt's prompt, where they are quoted as explicitly unverified
leads. If you surface them in a UI, label them that way — the whole reason this
is a separate table and a separate endpoint is that a half-explored spec
sitting in a review queue *looks finished*, which is worse than the lost run it
would replace. Promoting notes into a spec should stay a deliberate human act.

The task returns to `queued` and the attempt still counts against the cap, so a
scout that stops early at the same point every time is still retired after
three tries.

## Preserved bundles — a finished implementation with nowhere to go

A Builder VM is deallocated *before* the server pushes its branch, so when the
push or the PR is refused, the thin git bundle the VM sent is the only copy of
that implementation there is. It is written to
`<scratch_root>/rejected/<build_id>.bundle` and never swept, and the build's
`exit_reason` names both the file and the `git fetch` that recovers it.

`GET /bundles` (newest first) and `GET /builds/{id}/bundle`:

```json
{"build_id": "build_…", "path": "/…/rejected/build_….bundle", "bytes": 41234,
 "created_at": "…", "branch": "build/build_…", "base_sha": "…", "head_sha": "…",
 "exit_reason": "branch egress: …", "task_ids": ["task_…"],
 "recovery_command": "git fetch '/…/build_….bundle' 'build/x:build/x'",
 "superseded": false}
```

Four things a client should get right:

- **Show it against the tasks, not the build.** A build that never landed a
  branch has no PR and appears nowhere else in a UI; `task_ids` is on the wire
  type precisely so the block can hang off the work a human recognises.
- **Recovery does not happen in your app.** The file is on the server host and
  the fetch runs in whatever checkout the human works in, so render
  `recovery_command` in full and make it copyable rather than offering a
  Recover button that would have to lie about where it ran.
- **The bundle is thin.** It carries the build's commits and not the commit
  they grew from, so the fetch only reconstructs the branch in a repository
  that already has `base_sha` — state it beside the command.
- **Nothing is derived from a table.** The server reads the directory on every
  request, so a bundle a human `rm`s on the server host simply stops being
  listed. Don't cache the size.

Retention deletes **only reproduced work**, never by age and never by disk
usage: a bundle goes when every spec in its batch has been carried by a *later*
build that succeeded **and** every task in it is `done` — which in this system
means the issue closed upstream. A later build that has only opened a PR is not
evidence, because `watch_merges` can still find that PR closed unmerged and
unwind the batch, at which point the bundle is the head start again. So a
bundle for work nobody ever rebuilt is kept forever, and that is the intended
behaviour rather than a leak.

## Transcripts

Agent output is a **separate channel from the event log**, on purpose (see
*Event volume* below). Two owners — a scout session and a build — on one
contract, four endpoints:

- `GET /sessions/{id}/transcript?since=<seq>&limit=` — catch-up read. Returns
  `[{owner, seq, timestamp, stream, line}]`, oldest first. `stream` is
  `stdout` \| `stderr`. `since` is **inclusive**, matching `/events?since=` — a
  tailing client passes `last_seq + 1`. `limit` defaults to 500, capped at 2000.
- `GET /sessions/{id}/transcript/stream` — SSE tail. Replays from `since` (all
  of it, paging internally — no `limit`, so the stream can't silently skip a
  span) and then streams live lines, with the same 15s keepalive as
  `/events/stream`.
- `GET /builds/{id}/transcript?since=<seq>&limit=` and
  `GET /builds/{id}/transcript/stream` — the builder agent's output, same
  contract, same caps, same marker. When a build failed, this is the first
  thing to read: the build row says it failed, the transcript says why.

`owner` is internally tagged and names the route to fetch more from:
`{"kind":"session","session_id":"sess_…"}` or
`{"kind":"build","build_id":"build_…"}`.

`seq` is dense **per owner** and assigned server-side at persist time, so keep
cursors per owner, never per task — a build's first line is seq 1 no matter
what its specs' scout sessions recorded. An empty transcript means *nothing was
recorded* — runs predating transcript capture have none — so render it as "no
transcript", never as a failure.

Lines are the agent's stream-json output: one JSON object per line (assistant
messages, thinking, tool calls with their inputs, tool results, and a final
`result` record). Treat each `line` as opaque text unless you're prepared to
parse Claude Code's schema — it's forwarded verbatim, and a transcript can
contain any file the agent read, so it carries the same trust boundary as the
rest of this API: local SQLite, loopback only, no auth.

A few lines are the server's own, not the agent's, and they are plain text
rather than JSON: the truncation and drop notices below, and
`[tasks] builder agent exited with code N` — the builder agent's exit status,
written into the same ordered stream as the output that explains it.

`Session` also carries `usage` (`{input_tokens, output_tokens,
cache_read_input_tokens, cache_creation_input_tokens, total_cost_usd,
duration_ms, num_turns}`), parsed from that final record. Every field is
nullable and so is `usage` itself — a renamed upstream key costs a null, not a
failed scout. Builds have no equivalent column: a build's token cost is visible
inside its transcript, not on the build row.

**Credentials are scrubbed before a line is stored.** The server hands VMs a
`https://x-access-token:<token>@github.com/…` clone URL, and git echoes its
remote back — in `git remote -v`, in most of its errors — so agent output can
carry a live token. Every transcript line is rewritten on write, for both
owners: the userinfo of a `scheme://` URL's authority becomes `***`, giving
`https://***@github.com/o/r.git`. Only that userinfo is touched — an `@` later
in a path (`/a/path@with-at`) and an uncredentialed URL are left exactly as the
agent wrote them. Render lines verbatim as usual; a `***@` you didn't expect in
an old transcript is the one-time sweep that scrubbed rows written before this
landed, not something the agent printed.

Two caps bound each run server-side: 32 KiB per line (over-long lines are cut
and marked with a `[tasks: truncated ` prefix) and 8 MiB per run, after which
one notice is written and recording stops — the agent itself is unaffected.
Lines lost to queue pressure are announced inline as `[tasks] N transcript
line(s) dropped here` rather than left as an invisible gap.

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
  batched with others) → `awaiting_merge` (its PR is open and unresolved) →
  `done`, with `rejected` as the terminal failure state. A failed build
  returns `building → ready_to_build` — the spec is still good.
  Scout failures and `needs_revision` verdicts return to `queued`, never
  `backlog` — picked-up work stays picked up.
  One edge skips the middle: `backlog`/`queued` → `ready_to_build` when a
  human writes the spec themselves via `POST /tasks/{id}/build-now`. Switch
  on `to` rather than on the `(from, to)` pair and it costs nothing.
- **`done` means shipped, not "a PR exists".** A successful build parks its
  tasks in `awaiting_merge`; each poll reads the pull request at decision
  time and resolves it. Merged *and the merge commit is on the trunk* → the
  server closes the issue as completed, and the next poll retires the task to
  `done` through the ordinary closure-derived path. Closed unmerged → the
  batch's specs go back to `approved` (with a build attempt charged) and the
  tasks to `ready_to_build`, which restores the *option* to rebuild; nothing
  dispatches a build by itself. Still open → nothing happens, and the next
  poll asks again. `awaiting_merge` is **live work**: count it as active, and
  keep showing it even once its issue reads closed.
- **A PR's `merged` is not the test, and clients should not present it as
  one.** `merged` says the PR reached its *base*, and builds stack: a PR based
  on another build's branch reads merged the moment that branch takes it, and
  ships nothing until the branch itself lands. The server resolves
  `awaiting_merge` on whether the merge commit is an ancestor of
  `SCOUT_BASE_BRANCH`, so a batch can sit in `awaiting_merge` with a *merged*
  PR behind it — that is correct, not stale, and it stays that way until the
  stack lands (or a human unwinds it). It is never auto-unwound, because the
  legitimate stack order has the base merging afterwards.
- There is no "mark done" endpoint, deliberately: **closing the GitHub issue
  is the done signal.** When a picked-up task's issue closes, the next poll
  retires it — `queued`/`in_review`/`ready_to_build`/`awaiting_merge` become
  `done` (or `rejected` if the issue was closed as not-planned / duplicate),
  the task's `manual_rank` clears, and a normal `task_state_changed` event
  fires. A `scouting` task is left to finish first and retires from
  `in_review` on the following poll. Clients shouldn't offer a complete/done
  action; link to the issue instead.
- `Session.status`: `running`, `scout_succeeded`, `scout_failed`, `cancelled`.
- `SpecQueueEntry.status`: `pending_review`, `approved`, `needs_revision`,
  `blocked`, `rejected`, `built` (only the three verdict values are accepted
  by the review endpoint; `pending_review`/`blocked` are server-assigned, and
  `built` is how the approved queue drains — a successful Builder run
  assigns it, and a spec cannot be built twice).
- `Build.status`: `queued` → `running` → `succeeded` | `failed`.
- `Complexity`: `simple`, `medium`, `complex`.
- `Mode`: `play`, `pause`, `stop`.
- `Project.status`: `active`, `paused`, `archived`. `#[serde(default)]` on the
  wire — absent reads as `active`, which is what every project was before the
  column existed.

Parse enums leniently (unknown value → show raw string, don't crash): new
states will appear as the pipeline grows.

Event JSON: `{seq, timestamp, payload}` where payload is tagged by `"kind"`
(snake_case): `project_added`, `project_status_changed` (`project_id`,
`status` — the status is *in* the payload, against the usual identifier-only
rule, because a client that had to refetch `/projects` to learn which way the
switch moved could not narrate the event at all), `task_ingested`,
`task_state_changed`
(`from`/`to`), `task_gh_state_changed` (`task_id`, `gh_state` — the poller's
snapshot of GitHub's open/closed flag moved, most often because the issue
dropped out of the repository's open set; refetch the task or the list),
`session_started`, `session_completed`, `spec_created` (`spec_id`,
`task_id`, `session_id` — **nullable**, absent for a human-authored spec),
`spec_queue_status_changed`, `queue_reordered`, `spec_queue_reordered`,
`build_requested` (`build_id`, `spec_ids`), `build_started`,
`build_completed` (`build_id`, `status` — refetch the build for detail),
`run_cancel_requested` (`run_kind` ∈ `session` \| `build`, `run_id`, `actor`,
`decision_seq` — somebody asked for a run to stop; the *request*, not the
outcome, which arrives as that run's ordinary completion event with a
`cancelled` status),
`bundle_preserved` (`build_id`, `bytes` — a build's branch could not be
pushed and its commits were written down; at that moment the file is the only
copy of that implementation anywhere),
`bundle_removed` (`build_id`, `superseded`, `actor` — **`superseded` is the
whole difference**: `true` is the retention policy reclaiming work that has
since been rebuilt and shipped, `false` is somebody throwing an implementation
away, and the two want very different sentences in a feed),
`pull_request_opened` (`build_id`, `pr_number`),
`orchestrator_message` (`seq`, `role` — refetch `/orchestrator/messages`),
`mode_changed` (a `POST /mode` only — a boot's mode is set before the listener
binds and is reported as a `note` with `source: "startup"`, deliberately, since
`mode_changed` costs an orchestrator turn),
`note` (`source`, `message` — free-form breadcrumbs; a
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
  return to `queued` — clients just see the normal events. Since #835 an
  orphan that had checkpointed becomes `scout_stopped_early` instead, and its
  notes survive the restart.
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
