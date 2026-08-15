You are a Builder in the Double Diamond architecture.

You are implementing 3 approved spec(s). Each was written by a Scout that already explored the work by implementing it once in a throwaway branch you cannot see — the spec is the distilled result. Trust its pitfalls; verify its claims against the code in front of you.

## Spec 1 of 3: context_tokens measures per-invocation spend, not context size (#827)

## Spec: Split the orchestrator's context gauge from its per-tick spend (#827)

### Summary

`Orchestrator::context_tokens` summed the input side of the stream-json
`result` record, which aggregates usage across every internal turn of one
`claude --print` invocation — each of which re-reads the cached prefix. The
number it produced was cost-per-tick (2.7M on a live server), not context
size, and the doc comment asserted the opposite. This splits the two: context
size is now read from the **last main-chain `assistant` record's**
`message.usage`, which is the prompt behind a single model call and therefore
a genuine absolute reading; the `result` aggregate keeps being recorded, under
the name that describes it (`tick_tokens`). `orchestrator_sessions` gains a
column for each, migration `0016` renames the old column to `last_tick_tokens`
so its existing values keep their true meaning, and `last_context_tokens`
starts fresh (NULL) rather than reinterpreting numbers that were never a
context size. Item 7 of the orchestrator-mind plan can now build a rotation
threshold on `context_tokens` and get what it asked for.

### Implementation Approach

- **`crates/tasks/migrations/0018_orchestrator_usage.sql`** (new) — `RENAME
  COLUMN last_context_tokens TO last_tick_tokens`, then `ADD COLUMN
  last_context_tokens INTEGER`. The rename preserves the old values under an
  honest name (they *are* tick spend); the new column starts NULL because we
  never measured context, and seeding a rotation threshold with reinterpreted
  garbage is worse than seeding it with nothing. Migration `0012` is left
  untouched — sqlx checksums applied migrations, so editing history would
  break existing databases even though its comment now reads slightly dated.
- **`crates/tasks/src/orchestrator.rs`**
  - `StreamLine::Tools(Vec<String>)` becomes `StreamLine::Assistant { tools,
    context_tokens }` — the same record carries both the feed labels and the
    usage, so no new parse pass. `StreamLine::Result.tokens` is renamed
    `tick_tokens`.
  - New `TurnUsage { context_tokens, tick_tokens }` replaces the bare
    `Option<i64>` threaded through `invoke` → `Turn`.
  - `context_tokens()` is renamed `input_side_tokens()`: the arithmetic
    (`input + cache_read + cache_creation`) is unchanged and shared, because
    what differs between the two readings is not the sum but whose `usage`
    it is. Its doc comment now says exactly that.
  - Assistant records with a non-null `parent_tool_use_id` (sub-agent /
    sidechain turns) are excluded from the gauge — a `Task` sub-agent has its
    own conversation and its own context, so reading it would report a number
    unrelated to this session's memory. Their tool labels still reach the live
    feed; only the gauge filters.
  - Last main-chain reading wins within a tick: the final assistant turn is
    the context the next tick resumes from.
- **`crates/tasks/src/store.rs`** — `record_orchestrator_context_tokens` →
  `record_orchestrator_usage(cc_session_id, context_tokens, tick_tokens)`,
  writing both columns with `COALESCE(?, <column>)` so a `None` stalls the
  gauge instead of clearing it (a plain-text agent or test stub must not erase
  the last real reading). Early-returns when both are `None`. The three
  `SELECT`s (`orchestrator_sessions`, `orchestrator_session_info`,
  `orchestrator_session_row`) pick up the new column.
- **`crates/tasks-api/src/models.rs`** — `OrchestratorSessionInfo` gains
  `tick_tokens`; `OrchestratorSession` gains `last_tick_tokens`. Both
  `context_tokens` doc comments are rewritten to say what the value is, and
  the `tick_tokens` docs say explicitly that it is cost and must never be
  compared against a context window.
- **`docs/plans/2026-08-14-orchestrator-mind.md`** — item 1's claim that the
  `result` record is "an absolute reading of context size" is corrected in
  place and points at the split, so item 7 isn't built off the same mistake.
- **Tests** — the integration test `usage_separates_context_size_from_what_
  the_tick_spent` (renamed from `a_usage_reporting_agent_advances_the_context_
  gauge`) drives a fixture with two main-chain assistant turns, one sidechain
  turn with a 900k reading, and a `result` aggregate of 2.7M; it asserts
  `context_tokens == 182_000` and `tick_tokens == 2_702_000`, then runs a
  second tick with a usage-free stub and asserts neither reading is erased. A
  unit test in `orchestrator.rs` pins the parser rules directly (per-record
  reading, sidechain exclusion, missing-usage → `None`).

### Discovered Pitfalls

- **The two numbers share arithmetic but not meaning.** Both are
  `input_tokens + cache_read_input_tokens + cache_creation_input_tokens`. The
  bug was never the sum — it was reading it off the wrong record. Anyone
  refactoring should resist "deduplicating" the two call sites into one
  concept again.
- **Sub-agent records look identical to main-chain ones** apart from
  `parent_tool_use_id`. Without the filter, an orchestrator that dispatches a
  `Task` would report the sub-agent's context as its own, and the gauge would
  jump around for reasons unrelated to the session.
- **`0012`'s column comment is now stale but must stay.** Migrations are
  checksummed by sqlx; the correction lives in `0016`'s comment instead.
- **`GET /orchestrator/session` will report `context_tokens: null` on a live
  server until the next tick runs.** That is intended and is the honest state,
  but it means the first thing a human sees after deploying is a blank gauge,
  not a corrected number. `tick_tokens` carries the old value forward.
- **The gauge lags by one model call within a tick** when the last main-chain
  assistant turn is a tool call: its usage is the prompt *before* that tool's
  result was appended. The error is bounded by one tool result and is
  irrelevant against a rotation threshold; measuring after the fact is not
  possible from stream-json alone.
- **The reading excludes the final message's own `output_tokens`**, so it is
  "prompt size of the last model call" rather than "bytes in the context right
  now". Deliberate: it is a directly measured quantity rather than invented
  arithmetic, and the difference is a few thousand tokens against a threshold
  in the hundreds of thousands.
- **The build environment OOMs when linking test binaries in parallel** —
  `cargo test --workspace` was killed by the linker (signal 9). `CARGO_BUILD_
  JOBS=1 cargo test --workspace` completes. Not a code issue; noted so the
  Builder doesn't chase it.

### Blockers & Dependencies

None. Nothing else reads these fields yet — `app-gpui` doesn't render them,
and no endpoint exposes the session ledger — so this lands standalone. It is a
prerequisite for item 7 of `docs/plans/2026-08-14-orchestrator-mind.md`
(owned rotation), which is specified to trigger off `last_context_tokens`.

### Complexity

Simple

### Notes

- Wire-type change: `OrchestratorSessionInfo` and `OrchestratorSession` each
  gain a field. Per CLAUDE.md, clients ship from this repo, so `tasks-client`
  and `app-gpui` rebuild against it; no compatibility shim is warranted.
- Verified: `cargo clippy --workspace --all-targets` clean, `cargo fmt --all`
  applied, `CARGO_BUILD_JOBS=1 cargo test --workspace` fully green (all
  binaries plus doctests). `cargo-nextest` was not installed in this
  environment, so `make test-cargo` is the path that was exercised.
- If a rotation threshold is later added, read `context_tokens` and ignore
  `tick_tokens` entirely — the latter is a cost signal, useful for a budget or
  a "this tick was expensive" alert, never for a compaction decision.

## Spec 2 of 3: The orchestrator cannot present its actor token under a restricted allowlist, so the charter is silently inert (#826)

## Spec: Give the orchestrator an actor credential it can actually present (#826)

### Summary

The orchestrator's authority is enforced by attribution: the charter gates
`Actor::Orchestrator` and never `Actor::Human`, so a write the server cannot
attribute silently acquires *full human authority*. The old mechanism — a
`TASKS_ACTOR_TOKEN` env var interpolated into `-H "X-Tasks-Actor: orchestrator
$TASKS_ACTOR_TOKEN"` — could not be used under the default
`--allowedTools Bash(curl:*)`, because Claude Code refuses to statically verify
a Bash command containing a variable. The safest deployment was therefore the
one where the charter was inert. This replaces shell expansion with a
server-written curl config file (`<data dir>/orchestrator-curl.conf`, mode
0600) holding the header line; the prompt now tells the agent to pass
`-K <path>`, which is a static command, matches the allowlist, and keeps the
token out of argv (`ps`). The env var is removed rather than kept as a
fallback. Separately — the "worth considering" note in the issue — an
`X-Tasks-Actor` header that is *present but does not verify* is now refused
with 403 instead of being read as the human, so a broken credential is a
visible failure rather than an escalation.

### Implementation Approach

- `crates/tasks/src/store.rs`
  - New `pub const ACTOR_HEADER: &str = "X-Tasks-Actor"` — one statement of the
    header name, shared by the writer (orchestrator) and the reader (server).
    HTTP header lookup is case-insensitive, so the server's old lowercase
    literal is gone.
  - New `pub enum ActorClaim { Human, Orchestrator, Unrecognized }`.
    `Store::resolve_actor` now returns it: absent/empty header → `Human`
    (the human proves nothing), valid token → `Orchestrator`, anything else →
    `Unrecognized`. Also trims, so a header whose token half is whitespace
    cannot pass.
- `crates/tasks/src/server.rs`
  - `actor_of` returns `ApiResult<Actor>` and maps `Unrecognized` to a 403
    naming the expected form. All six call sites take `?`. Rationale in the
    doc comment: since the human is never gated, demoting a failed claim to
    "human" hands the caller *more* authority than it asked for.
- `crates/tasks/src/orchestrator.rs`
  - `OrchestratorConfig` gains `curl_config: PathBuf`.
  - `curl_config_contents(token)` renders the file: comment header plus one
    option line, `header = "X-Tasks-Actor: orchestrator <token>"`.
  - `write_curl_config(path, token)` creates the parent, writes a sibling
    `.tmp` opened `create_new` with mode 0600 (never chmod-after: the gap
    would be small, but the file is a credential), then renames — so a turn
    never reads a half-written config. A leftover `.tmp` from a crashed write
    is removed first, since `open` will not lower an existing file's mode.
  - `invoke` rewrites the file before every spawn (the token is minted per
    boot) and fails the turn via a new `OrchestratorError::ActorConfig` if it
    can't — an agent that cannot identify itself must not run. The child env
    now gets `.env_remove("TASKS_ACTOR_TOKEN")` instead of `.env(...)`.
  - `system_prompt` takes the config path and splices it into a rewritten
    attribution paragraph: pass `-K <path>`, don't read/print/copy it, don't
    aim it at any host but `127.0.0.1:<port>` (curl would send the header
    wherever you point it), and if you can't make an identified write, say so
    and stop rather than making an unidentified one — which "is recorded as
    the human's". A worked example line shows the full shape with `rationale`.
- `crates/tasks/src/run.rs`
  - `Config::orchestrator_curl_config()` → `<data dir>/orchestrator-curl.conf`,
    wired into `OrchestratorConfig` in `orchestrator_loop`.
- `CLAUDE.md` — new load-bearing design rule: the charter only binds what the
  server can attribute, so attribution must work under the *tightest* agent
  permissions; never move that credential into argv, the prompt, the
  environment, or the agent workdir.

Tests (all real processes / real HTTP, per repo convention):

- `tests/orchestrator.rs::the_agent_identifies_its_writes_with_the_curl_config_and_no_shell_expansion`
  is the regression test for #826 and is deliberately end-to-end: a stub agent
  parses the `-K <path>` out of the system prompt it was handed, runs a real
  `curl -K` against a real bound server to approve a spec, and the test asserts
  the ledger row says `orchestrator` and `enforced`, plus that the file is 0600
  and outside the workdir. A unit test cannot catch this class of bug — the old
  scheme passed every unit test and still couldn't be run by the agent.
- `orchestrator.rs` unit tests: the prompt asks for `-K` and contains no
  `$TASKS_ACTOR_TOKEN`; the config file is exactly one curl option; the writer
  is 0600, replaces in place, and leaves no temp file.
- `store.rs::a_failed_actor_claim_is_not_the_human` — stale token, empty
  expansion, bare word, wrong role all resolve `Unrecognized`.
- Updated `the_actor_header_decides_who_a_write_belongs_to`: a wrong token is
  now 403 (was 200-as-human); a request with *no* header is the human.

`make test`: 320 passed, 0 failed (2 expected LEAKs, the documented scout
timeout tests), doctests green. `cargo clippy --workspace --all-targets` and
`cargo fmt` clean.

### Discovered Pitfalls

- **Do not put the credential in the orchestrator workdir**, which is what the
  issue's fix direction suggested. In production `ORCHESTRATOR_WORKDIR` points
  at a repo checkout the agent commits from, so a secret there is one
  `git add -A` from being published. It lives under the data dir instead and
  the absolute path is spliced into the prompt.
- **`-K` is not scoped to a host.** Everything in the config applies to
  whatever URL that curl invocation names, so the file must hold *only* the
  header (a unit test pins that) and the prompt has to forbid using it against
  anything but the loopback API. Mitigating factor: the token is minted per
  boot and held only in memory, so a leaked one dies with the process.
- The prompt mentions the path twice (prose + worked example), and the prose
  mention is inside backticks. Anything parsing the path back out of the prompt
  must exclude backticks from the match — the test stub hit this.
- Making a failed claim a 403 is a wire-behaviour change. Nothing in the repo
  sends `X-Tasks-Actor` except the orchestrator and tests (app-gpui and
  tasks-client send none, so they stay the human), but any external script that
  was sending a junk header and getting human authority will now get 403 —
  which is the point.
- `env_remove("TASKS_ACTOR_TOKEN")` is deliberate, not just "stop setting it":
  the server's own environment could carry one, and inheriting it would revive
  the exact fallback path this removes.
- The briefing agent (`BRIEFING_CMD`) is read-only and gets no config file;
  don't "helpfully" hand it one.

### Blockers & Dependencies

None. Builds on #823 (`orchestrator_charter`, the decisions ledger, the
per-boot actor token) which is already on `main`.

### Complexity

Medium — small diff, but it touches the authority path, so the reasoning
matters more than the code.

### Notes

- The failure is asymmetric and that shapes every choice here: a credential
  that *fails closed* costs a turn, one that *fails open* silently grants human
  authority, bypasses the charter, misattributes the ledger, and misfires the
  echo filter. Hence: one mechanism with no fallback, a turn that aborts if the
  file can't be written, and a 403 on a claim that doesn't verify.
- The e2e test shells out to `curl` (already present alongside `git`, which
  other tests exec). That dependency is intentional: it validates curl's own
  config-file syntax, which is the part a hand-rolled assertion would get wrong.
- A stale `orchestrator-curl.conf` survives a restart, but its token doesn't —
  it's rewritten before the next turn and useless in between. Not worth
  deleting on shutdown.
- Possible follow-up, not done here: surface the config path on
  `GET /orchestrator/session` next to the workdir, so a human resuming the
  session interactively has the same instructions in front of them. The prompt
  already carries the absolute path, so it works today.

## Spec 3 of 3: Builder deallocate is unbounded on the failure path, blocking the serial build queue for hours (#824)

## Spec: Bound VM teardown, and stop charging it to the agent

### Summary

`build_5c65e18a` hit its 3600s budget on schedule and then spent 84 minutes
inside `self.client.deallocate(&vm_id)`, holding the serial build queue and
writing nothing to the event log. Two independent defects: the dispatchers
wrapped only the *drain* in `tokio::time::timeout`, leaving a request/response
round-trip to vm-pool unbounded on every path including failure; and
`completed_at` is stamped by the finalizers, which run after teardown (and, on
success, after the push and the PR), so `completed_at - started_at` was never
the interval the budget bounds and disagreed with `exit_reason` by
construction. The fix is a shared `crate::teardown::deallocate_bounded` —
teardown gets its own small budget and abandoning it is a logged event rather
than silence — plus a new `builds.agent_finished_at` column stamped the moment
the drain ends, before teardown. Both dispatchers use the bounded call: the
Scout had the identical unbounded `deallocate`, where a hang holds a scout
concurrency slot instead of the whole build queue.

### Implementation Approach

- **`crates/tasks/src/teardown.rs` (new, private module, wired into
  `lib.rs`).** `DEALLOCATE_TIMEOUT: Duration = 120s` and
  `deallocate_bounded(client, store, vm_id, owner, timeout) -> bool`. Takes the
  timeout as a parameter rather than reading the constant internally, so the
  test can drive the expiry path in milliseconds; both call sites pass
  `DEALLOCATE_TIMEOUT`. Never returns an error to the caller — a dispatch's
  outcome belongs to its agent, not to how tidily the VM went away. The error
  path warns; the expiry path warns *and* appends an `EventPayload::Note` under
  the existing `run::DISPATCHER` source, because a teardown that hangs is
  invisible from the outside otherwise, which is precisely what made this
  incident hard to read.
- **`crates/tasks/src/builder.rs`.** Three changes in `attempt`: `warn!` at the
  drain-timeout branch itself (previously the only log was `dispatch`'s failure
  warn, which is *after* teardown — hence 15:37 → 17:50 of silence); stamp
  `set_build_agent_finished(&build.id, Utc::now())` right after the drain
  result is known and before teardown, best-effort so a store hiccup cannot
  skip the deallocation; and route the deallocate through
  `deallocate_bounded`. The stamp sits after the `let result = …` match, so it
  covers the send-error, drain-failure, and timeout paths as well as success.
- **`crates/tasks/src/scout.rs`.** Same `deallocate_bounded` call, `owner`
  string `"scout for task <id>"`. No timestamp change — see Notes.
- **`crates/tasks/migrations/0017_build_agent_finished.sql`.** `ALTER TABLE
  builds ADD COLUMN agent_finished_at TEXT` — nullable, since a build that
  never reached an agent has no phase to stamp and every pre-existing row
  predates the field.
- **`crates/tasks-api/src/models.rs`.** `Build::agent_finished_at:
  Option<DateTime<Utc>>`, placed between `started_at` and `completed_at`. No
  `#[serde(default)]`, matching the crate's stated position that clients ship
  from this repo and skew is a build error.
- **`crates/tasks/src/store.rs`.** New `set_build_agent_finished`; the column
  added to both build `SELECT` lists (`get_build`, `list_builds`) and to
  `build_from_row`; the two `Build` literals updated. The finalizers are
  untouched — they must not overwrite the stamp.
- **`docs/clients.md`.** The `/builds` entry now says a build has *two*
  durations and which one to render as "took".

### Discovered Pitfalls

- **`build_from_row` reads by column name, so forgetting either `SELECT` is a
  runtime error, not a compile error.** There are exactly two build selects
  (`get_build`, `list_builds`); the added store test asserts the field survives
  the list path specifically, because the by-id path is the one integration
  tests exercise and the list path is the one that would silently rot.
- **Abandoning a `deallocate` leaks one entry in the vm-pool client's in-flight
  request table** (`Connection::pending`): `request()` inserts the oneshot
  sender before awaiting, and cancelling at the await leaves the map entry
  until the response arrives or `close()` clears it. Bounded by connection
  lifetime and one entry per abandoned teardown, versus an unbounded stall —
  the trade is deliberate, and noted in the module doc so nobody "fixes" it by
  removing the timeout.
- **Freeing the VM is vm-pool's job either way.** Walking away is exactly the
  state the pool already handles when the server is killed mid-call; its health
  loop reaps VMs the server stops tracking. Nothing in tasks needs to retry.
- **The timeout is additive to the run budget** — a build can now take
  `BUILDER_TIMEOUT_SECS + 120s + egress`. Bounded, which was the point, but it
  means `DEALLOCATE_TIMEOUT` must stay small relative to the run budget rather
  than being treated as a second budget to tune.
- **Do not fold the deallocate timeout into `exit_reason`.** The exit reason
  describes the agent phase; a teardown that expired after a *successful* build
  must not make the build look failed. That is why it is an event-log note.
- **The `Instant`-based `remaining` budget already handled the
  already-blown-budget case** (`saturating_sub`); nothing there needed changing
  and it is easy to "helpfully" rewrite it into a subtract that wraps.
- **`EventPayload` is exhaustive over `kind()` by design** — a new variant
  would have forced a client-vocabulary conversation. Reusing `Note` avoids
  that for what is genuinely a breadcrumb for humans watching the stream.

### Blockers & Dependencies

None. Self-contained in `crates/tasks` + one `tasks-api` field; no GitHub
interaction, no vm-pool change (the crate stays pure infrastructure — the
deadline is imposed by the caller, not added to the client).

### Complexity

Simple

### Notes

- **Tests, all no-mock, all green** (`make test`: 317 passed, plus doctests;
  the two `LEAK`s are the documented pre-existing scout-timeout ones):
  - `teardown::tests::a_deallocate_that_never_answers_is_abandoned_and_said_so`
    — binds a real `UnixListener` that accepts, reads every request line, and
    answers none; connects the real `Client<TasksProtocol>` to it. The client,
    the socket, and the framing are real; only the service's *silence* stands
    in. Asserts the call returns inside its budget, reports not-acked, and
    leaves exactly one `Note` naming the vm id and the owner.
  - `store::tests::the_agent_phase_ends_on_its_own_clock` — a claimed build has
    `agent_finished_at == None`; after a stamp and a `finalize_build_failed`,
    the stamp is unchanged and `completed_at >= agent_finished_at`; the value
    survives `list_builds`.
  - `tests/builder.rs` — both existing end-to-end tests now assert the clock
    ordering, including the failure path, which is the one that hung.
- **Scout sessions have the same timestamp shape and were deliberately left
  alone.** `finalize_succeeded`/`finalize_failed` stamp `sessions.completed_at`
  with `Utc::now()` after teardown, so a scout's stored duration includes
  teardown too. With teardown now bounded the skew is bounded, and fixing it
  properly means threading the drain-end time through both finalizers (where
  the same `now` is also the spec's `created_at`) — a real change to a second
  table for a symptom nobody has reported. Worth a follow-up issue, not this
  one.
- **`DEALLOCATE_TIMEOUT` is a constant, not an env var.** The config surface is
  already large and this is an infrastructure sanity bound, not an operating
  choice; if a real pool is ever slow enough to need it tuned, that is a
  vm-pool bug worth seeing rather than a knob worth turning.
- The app (`app-gpui`) is not a workspace member but path-deps `tasks-api`, so
  it recompiles against the new field. It only deserializes `Build` and
  constructs no literals — nothing there needs editing, though a build-detail
  view showing "took" should read `agent_finished_at - started_at`.

## Your job

1. Implement every spec above, in order, as one coherent change in the cloned repo (cwd). You are on the right branch already.
2. Run the project's tests / lint / typecheck — get them green.
3. Commit your work with clear messages (a git identity is configured).
4. Write `SUMMARY.md` in the repo root: one or two paragraphs describing the change, suitable as a pull request body. Do not use GitHub closing keywords (`Closes #N`, `Fixes #N`) — the server links the issues itself.
5. Do NOT push and do NOT open a PR — the server does both.
