# Tasks

An autonomous software pipeline with a human in the loop. Tasks turns a
GitHub issue tracker into shipped pull requests by orchestrating headless
[Claude Code](https://claude.com/claude-code) agents — and keeps a human in
the position of *reviewer*, not operator.

<!-- disclaimer:start -->
## Read this first

Tasks runs coding agents against your repositories with nobody in the loop
between a decision and the act. Before you point it at anything, know what it
does.

- **The agents run unattended, with permission checks off.** Scouts and
  Builders are Claude Code processes started with
  `--dangerously-skip-permissions` (`images/scout/Dockerfile`,
  `images/builder/Dockerfile`): inside their VM they run whatever commands
  they decide to run, and nothing asks you first. The VM is the boundary, and
  it is a real one — each is ephemeral, and the credentials a run is given
  reach Anthropic and read the one repository it was dispatched for. Agents
  cannot push.
- **The server can.** On an agent's say-so, under its own GitHub credential,
  while you are not watching, it pushes branches, opens pull requests, merges
  them, comments on issues and closes them. All nine capabilities in the
  orchestrator's charter ship `live` and uncapped
  (`crates/tasks/migrations/0016_charter_live.sql`) — no daily limit, no
  pre-approval gate, nothing that stops at the tenth merge of the day. The
  charter is a kill switch, not a promotion ladder, and the append-only
  decisions ledger is something you read afterwards. Any one capability can be
  switched off on its own, which is how you keep the pipeline and merge by
  hand:

  ```sh
  curl -X POST localhost:4800/charter/land_builds \
       -H 'content-type: application/json' -d '{"level":"off"}'
  ```

- **The local API has no authentication.** Anything running on your machine
  that can reach port 4800 can drive the pipeline — start a Scout, start a
  Builder, merge a pull request — and it is recorded as you, because a caller
  that does not identify itself is read as the human and the human is never
  gated. Web pages are refused (`crates/tasks/src/loopback.rs`), but that
  guard is about pages, not about processes.
- **The orchestrator is not in a VM.** It is a Claude Code agent in an
  ordinary child process on the machine running the server, and what it may
  do is whatever `ORCHESTRATOR_CMD` allows. The default is `curl` and nothing
  else; pointing `ORCHESTRATOR_WORKDIR` at a checkout and adding
  `--dangerously-skip-permissions`, which `CLAUDE.md` describes as a
  supported way to run it, makes it a full developer agent on your host.

The server boots paused, so nothing moves until you say so. Point it at a
repository you would not mind rewriting — a fork, a scratch repo, something
where a bad pull request costs you a click. This is software one person wrote
to run on his own machine, published in case it is useful to you: there is no
warranty, nobody is on call, and if it breaks something of yours, fixing it is
your job. The `LICENSE` says the same thing in the legal register.
<!-- disclaimer:end -->

## The idea

Most agent tooling makes you drive: prompt, watch, approve, repeat. Tasks
inverts that. Work moves on its own — issues are ingested, explored, specced,
built, merged, and closed — while every judgment call lands in an append-only
decisions ledger you can audit and reverse. Oversight happens after the fact,
where it's cheap, instead of as pre-approval gates, where it's the bottleneck.

The architecture is a double diamond:

```
GitHub issues ──▶ backlog ──▶ queue ──▶ SCOUT ──▶ spec ──▶ review ──▶ BUILDER ──▶ PR ──▶ merged ──▶ done
                                     (parallel)          (human or            (serial)
                                                        orchestrator)
```

- **Scouts** run in parallel, each in its own ephemeral VM. A Scout explores
  the codebase and writes a **spec** — a text document saying what to build
  and how. The spec is the deliverable; the Scout's code is thrown away.
- Specs land in a **review queue**. A human (or the orchestrator, when
  granted) approves, requests revision, or rejects.
- **Builders** run serially, one at a time. A Builder gets approved specs —
  never Scout code, the information barrier is deliberate — implements them
  on a fresh branch, and opens a PR with its own test verdict.
- A merge watcher confirms the work actually reached trunk (not just that
  GitHub said `merged`), then closes the issue. `done` always means
  *shipped*.

Sitting beside the pipeline is the **orchestrator**: a Claude Code agent that
wakes on a tick, reads a brief of everything owed a decision, and acts —
queueing tasks, reviewing specs, dispatching and landing builds, commenting
upstream. What it *may* do is governed by a nine-capability **charter**
(each `off` / `shadow` / `live`, human-writable only), enforced server-side
and regenerated into its prompt every turn. The charter is a kill switch,
not a promotion ladder: everything ships live, and the ledger is the safety
mechanism.

GitHub stays at the edges. Issues in, PRs out, nothing GitHub owns is ever
cached — mergeability, CI, open/closed are queried at decision time.

## The pieces

| | |
| --- | --- |
| `crates/tasks` | the server: SQLite store, GitHub polling, dispatchers, HTTP API + SSE, orchestrator loop |
| `crates/tasks-api` | wire types shared by server and clients |
| `crates/tasks-protocol` | Scout/Builder command protocol |
| `crates/scout-supervisor` | PID 1 inside agent VMs: clone, run the agent, stream results back |
| `crates/vm-pool` | vendored VM infrastructure (apple/container); app-agnostic, independently publishable |
| `images/` | the Scout/Builder container images |
| `app-gpui` | native Mac app: three-pane workspace — task queue rail, tabbed task view (overview, brief, live agent feed, changes), always-on orchestrator chat |

## Running it

Read [Read this first](#read-this-first) before you point this at a
repository — it is the section about what runs unattended, and what the server
is allowed to do to your GitHub while you are not looking.

### Prerequisites

**To build and test.** This half works on Linux as well as macOS, `app-gpui`
included:

- Rust, edition 2024.
- `cargo install cargo-nextest --locked`, for `make test`. `make test-cargo`
  is the fallback that needs nothing.

**To run the pipeline.** macOS only, because the VMs are apple/container's:

- [apple/container](https://github.com/apple/container), *and* its services
  started — `container system start`. The CLI being installed is not the same
  fact as the runtime running, and only the second one starts a VM.
- `rustup target add aarch64-unknown-linux-gnu`, plus the cross linker
  (`brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu`).
  Both are needed only by `make images`.
- **A GitHub token.** With none, polling is disabled and the pipeline has no
  intake at all. `.env.example` names the exact scopes.
- **An Anthropic credential.** This is the one that fails quietly. Nothing
  warns at boot, the server comes up looking healthy, and every scout then
  dies inside its VM on agent auth — the recognisable symptom is a 502 from
  the broker reading `the host has no anthropic key configured`. If you
  already use Claude Code with an `apiKeyHelper` at
  `~/.claude/anthropic_key.sh`, that is the third resolution step and may
  already be answering for you, which is exactly why it surprises the second
  machine.

`tasks doctor` asks all of this and more, in the order the preconditions
bite. `make check-toolchain` is the narrow version — cross linker, Rust
target, `container` on `PATH` — and it deliberately cannot see
`container system start` or either credential.

### Setup

Once, in a checkout:

```sh
cargo build -p tasks
export PATH="$PWD/target/debug:$PATH"   # every `tasks …` line below

tasks secrets init                     # sealed credential store; unseal key goes to the Keychain
                                       #   (--key-file PATH is first-class, not a fallback:
                                       #    an unsigned dev build is a new application to a
                                       #    macOS access list on every rebuild)
tasks secrets set anthropic-api-key    # value on stdin: paste, then ctrl-D
tasks secrets set github-token         # ...and the same for this one
cp .env.example .env                   # everything that is *not* a credential
cargo run -p tasks -- add-project owner/repo
make images                            # the Scout/Builder VM images
```

`target/debug/tasks` is not a convenience copy: it is literally what `make
serve` and `make restart` run (`TASKS_BIN` in the `Makefile`), so putting it on
`PATH` leaves one binary and nothing that can drift. `tasks service install` is
the other shape — one LaunchAgent, login and crash restart — and it is for
leaving this running rather than for trying it out: with a managed server up,
`make serve` refuses outright, since `--foreground` would race the service for
the port.

`make images` has no progress bar and takes a while. It cross-compiles three
supervisors in release to `aarch64-unknown-linux-gnu`, runs four
`container build`s in sequence (`vm-pool-base` → `vm-pool-agent` → `agent:v1`
and `builder:v1`), and finishes by booting each image to read its `--version`
back. Long silences are the cross-compiles; it has not hung. Running it here,
before anything is serving, is safe — it gates on `tasks drain --check`, which
passes when nothing is serving. Once a server *is* up, that gate is real, and
the sequence is the one under **Day to day**.

Keys never reach a VM: agents run on short-lived, repo-bound leases redeemed
through an in-process broker, and the sealed store means no raw key sits in
`.env` either (raw `GITHUB_TOKEN` / `ANTHROPIC_API_KEY` env vars still work
as fallbacks, warned at startup, and `.env.example` documents them as exactly
that). See `docs/plans/2026-08-18-credential-custody.md`.

### First run

Three terminals. The first two block, and `PATH` is per-terminal — re-export
it, or use `./target/debug/tasks`.

```sh
tasks vm-pool                          # 1: the VM pool, a separate long-lived daemon
make serve                             # 2: the server, logging here
tasks doctor                           # 3: every precondition for a scout, as a
                                       #    checklist; exit 1 on any failure
```

With `tasks doctor` clean, the issues are already in: the first GitHub poll
runs at startup rather than one interval into it, and a paused server still
polls. (Add a project *after* the server is up and it is the next poll —
`TASKS_POLL_INTERVAL`, 60s.) Now queue something and start the pipeline:

```sh
curl -s localhost:4800/tasks | jq -r '.[] | "\(.id)  #\(.gh_issue_number)  \(.state)  \(.title)"'
curl -X POST localhost:4800/tasks/<task-id>/queue
curl -X POST localhost:4800/mode -H 'content-type: application/json' -d '{"mode":"play"}'
```

Both of those last two lines are needed, for two independent reasons, and
skipping either leaves a healthy-looking server that dispatches nothing:

- **Ingested issues land in `backlog`, and backlog is never dispatched from.**
  Only explicitly queued tasks reach a Scout. That is what makes intake safe —
  adding a repo with 10,000 issues creates 10,000 rows and zero VM runs — and
  it is not a thing that times out into happening.
- **Every boot comes up `pause`d**, overwriting whatever mode the last process
  left in the store. Intake and the API run; nothing dispatches. Say `play`
  here, or `TASKS_DEFAULT_MODE=play` in `.env` for a host you want dispatching
  unattended.

The app (`app-gpui`, built separately — `cd app-gpui && cargo run`) does all
three of those, and is where the loop actually closes: watch a Scout's live
Claude Code stream, read its spec, approve it, and talk to the orchestrator
about why it did what it did.

### Day to day

```sh
make restart      # upgrade a running server in place (drain, swap, verify)
make status
tasks doctor      # preflight: every precondition for a scout, with its fix
make test         # real processes, real SQLite, no mocks
```

`tasks doctor` is the answer to "can this machine actually run a scout?" — it
asks the container CLI and its services, vm-pool's socket and both its
ledgers, the images, the sealed store, the credential broker VMs redeem leases
against, GitHub's answer to your token, and the rest, in the order the
preconditions bite, and names the command that fixes each failure. It reports
and never fixes, and it writes nothing.

Host work the server cannot do to itself — restarting vm-pool, rebuilding
images — wants the pipeline quiesced first, and the hold outlives the command:

```sh
make drain        # hold dispatch, and wait for work in flight to land
make images       # ...then the host work: this, or a vm-pool restart
make resume       # ...then give dispatch back
```

### When something is wrong

| symptom | check | fix |
| --- | --- | --- |
| anything, or you don't know yet | `tasks doctor` | whatever it names — every failing check prints the command that changes it |
| doctor is clean, mode is `play`, nothing dispatches | `curl -s localhost:4800/tasks \| jq -r '.[] \| "\(.id) \(.state)"'` — is it `backlog`? | `POST /tasks/{id}/queue`; backlog never dispatches on its own |
| doctor warns that dispatch is held | `make status` names which hold: GitHub not answering, an update pending, or a full pool | each clears on its own terms — a GitHub hold on the next good poll, an update hold on `make restart` / `make images`, a pool hold on the next VM handed back |
| a supervisor change seems to have no effect | `make images-check` (or `tasks doctor --probe-images`) — the images are rebuilt by hand and nothing does it for you | `make drain` → `make images` → `make resume` |
| a scout started and then died | its transcript, in the app or `GET /sessions/{id}/transcript`, and the `exit_reason` on the session | nothing, usually: a dropped API connection is resumed in the VM and costs the task no attempt. Only a run that reached a verdict is charged one, and three of those reject the task |

## Reading further

- `CLAUDE.md` — the load-bearing design rules, with the reasoning attached
- `docs/plans/` — implementation plans, including the v2 architecture and
  the v3 UI spec
- `crates/vm-pool/CLAUDE.md` — vm-pool's own conventions

## License

MIT. `Copyright (c) 2026 Nate Butler` — a single year, because every commit in
this repository is dated 2026 (the first is 2026-03-11) and a range would
claim authorship years that do not exist.

`crates/vm-pool/` carries its own copy of that text, and so does each of the
three crates `cargo publish` can reach (`vm-pool-protocol`, `vm-pool-client`,
`vm-pool-manager`). That is not redundancy: `cargo package` ships only files
under the *package* root and never walks up, so a published crate whose
LICENSE lived in a parent directory would reproduce exactly the bug this file
is about — a `license` field with no text behind it. The three vm-pool crates
carrying `publish = false` deliberately get no copy; adding one would imply a
publish path that is switched off.

### Third-party

The app links Apache-2.0 code: `gpui-unofficial` (and its platform crate) is
`license = "Apache-2.0"` and ships `LICENSE-APACHE` in the published artifact,
and `gpuikit` is `MIT OR Apache-2.0` at the pinned rev — taken here under its
MIT arm, which is directly compatible with licensing this repository MIT.
GitHub's detector reports `gpuikit` as Apache-only because it picks one of a
dual pair, and the `gpui-unofficial` *repository* has no LICENSE file at all
because it is release automation with no gpui source in it; neither is a
blocker on redistributing a built `Tasks.app`.

Apache-2.0 §4(a) asks that a copy of the License travel with a binary
distribution, so `make app`/`make dist` copy `app-gpui/third-party/` into
`Tasks.app/Contents/Resources/third-party/` and the bundle's
`NSHumanReadableCopyright` points there rather than claiming MIT for the whole
artifact. That directory covers the crates `app-gpui/Cargo.toml` declares
directly, read from the artifacts the build consumes; a full **transitive**
audit (`cargo about` / `cargo-deny`) is still owed and is its own issue.
