# Operating a Tasks server

Setup, first run and day-to-day are in the [README](../README.md); this is the
reference beside it. It is deliberately an *index* rather than an explanation:
where a knob's behaviour is subtle, the reason lives in the doc comment on the
module that reads it, which is the copy that gets updated when the behaviour
does.

Data dir: `~/.local/state/tasks-v2/` (override: `TASKS_DATA_DIR`).

## Configuration

Config is read from `.env` as well as from the environment
(`crates/tasks/src/env_file.rs`). Three files are tried, in this order, and the
first to define a variable wins — with the real environment outranking all of
them, so `GITHUB_TOKEN=… tasks serve` still overrides:

1. `<data dir>/.env` — launcher-independent, and the only one an installed
   binary outside a checkout can have
2. the nearest `.env` at or above the **cwd** — a developer's `make serve`
3. the nearest `.env` at or above the **executable** — the same repo file,
   found when the cwd is `/` because launchd started the app

The third is not redundant. Configuration used to come from the process
environment alone, so it only ever applied to a server started from a shell
that had exported it: restarting from the app's Server menu — whose ancestor is
launchd — silently reverted `GITHUB_TOKEN`, `ORCHESTRATOR_CMD` and
`ORCHESTRATOR_WORKDIR` to their defaults, and the server came up healthy and
wrong.

`Command::env_remove` is the opposite of a scrub: the real environment is the
only thing a `.env` entry loses to, so removing a variable from a child's
environment *promotes* the file that defines it. Tests that exec the `tasks`
binary set `TASKS_ENV_FILES=off` for that reason.

### Environment variables

| var | default | |
| --- | --- | --- |
| `TASKS_SERVER_PORT` | 4800 | HTTP API port (also `--port`) |
| `TASKS_POLL_INTERVAL` | 60 | seconds between GitHub polls. Also sets how long a GitHub dispatch hold survives a dead poll loop |
| `TASKS_DEFAULT_MODE` | `pause` | the mode **every** boot starts in, overwriting whatever the last process left in the store. An unparseable value refuses to boot. Only `tasks reload` overrides it, by carrying the old server's mode to the new one |
| `TASKS_ENV_FILES` | `on` | `off` skips `.env` loading entirely, for tests that exec the binary. Anything else refuses to boot |
| `TASKS_INTAKE_LABEL` | — | when set, only open issues carrying that label are ingested (case-insensitive). Applied after the fetch, so closure tracking still sees the complete open set |
| `SCOUT_MAX_CONCURRENT` | 2 | scouts running at once. See *Pool capacity* |
| `SCOUT_IMAGE` | `agent:v1` | vm-pool image scouts run in |
| `BUILDER_IMAGE` | `builder:v1` | vm-pool image builds run in. `make images` rebuilds both |
| `SCOUT_TIMEOUT_SECS` | 3600 | budget per scout. Keep below vm-pool's `vm_timeout` (7200) |
| `BUILDER_TIMEOUT_SECS` | 3600 | budget per build, allocation included. Same ceiling argument |
| `SCOUT_CHECKPOINT_INTERVAL_SECS` | 30 | how often a Scout's `NOTES.md` is re-read and streamed back. Read *inside* the VM — set it in `images/scout/Dockerfile` |
| `SCOUT_MAX_RESUMES` / `BUILDER_MAX_RESUMES` | 2 | times a supervisor re-invokes an agent with `--resume` after its API connection dropped. `0` disables. Read inside the VM |
| `BUILDER_SUITE_BUDGET_SECS` | derived | hard cap on the in-VM `.tasks/verify` run; `0` skips it and reports `Unavailable`, which is never green. Read inside the VM |
| `SCOUT_VM_CPUS` / `SCOUT_VM_MEMORY_MB` | 4 / 6144 | shape of a Scout VM, multiplied by `SCOUT_MAX_CONCURRENT` |
| `BUILDER_VM_CPUS` / `BUILDER_VM_MEMORY_MB` | 4 / 8192 | shape of a Builder VM. Larger because builds are serial |
| `SCOUT_BUILD_JOBS` / `BUILDER_BUILD_JOBS` | derived | `CARGO_BUILD_JOBS` per VM, derived from VM memory because cargo's `-j` default knows nothing about the memory limit |
| `VM_POOL_SOCKET` | `/tmp/vm-pool.sock` | vm-pool service socket. A start against an occupied socket refuses rather than taking it over |
| `VM_POOL_MAX_VMS` | 6 | VMs the pool holds at once. Read by `tasks vm-pool`, never by the server, so a change needs a *pool* restart |
| `TASKS_VM_POOL_AUTOSPAWN` | derived | whether a failed vm-pool connect spawns the pool from the serving binary. Unset, derived from whether the binary is installed or a checkout artifact |
| `GITHUB_TOKEN` | — | **fallback** for `tasks secrets set github-token`, warned at startup. The sealed store is where production keys live |
| `GITHUB_API_URL` | api.github.com | GraphQL endpoint override |
| `GITHUB_OAUTH_URL` | `https://github.com` | where the device flow speaks to GitHub — both `tasks auth login` and the server's own sign-in surface (`POST /auth/github/device`, human-only, driven from the app's Server window; the server polls and seals, so the token never transits the app). Deliberately not `GITHUB_API_URL` — the OAuth endpoints are on github.com. Override for tests only |
| `GITHUB_CLONE_URL_BASE` | `https://github.com` | clone URL prefix, and where the broker forwards git traffic. A non-http(s) base cannot be proxied and clones direct |
| `TASKS_BROKER_PORT` | 4801 | credential broker listener, where VMs redeem run leases. A second listener on purpose; the API stays loopback-only |
| `TASKS_BROKER_BIND` | `0.0.0.0` | broker bind address. All interfaces because the vmnet gateway does not exist until the first container starts; every route demands a live lease |
| `TASKS_BROKER_ADVERTISE` | `192.168.64.1` | the broker's address as VMs see it. Also what the dispatch gates and `tasks doctor` probe — never loopback, which answers correctly while the gateway is severed |
| `TASKS_BROKER_ANTHROPIC_UPSTREAM` | `https://api.anthropic.com` | where Anthropic traffic forwards. Override for tests only |
| `TASKS_SECRETS_KEY_FILE` | — | unseal-key file, outranking the credential-store item. First-class on macOS, not a fallback: an unsigned dev build is a different application to an access list on every `cargo build` |
| `ORCHESTRATOR_CMD` | `claude --print … --allowedTools Bash(curl:*)` | orchestrator agent command; its permission flags decide what the orchestrator may do. Split shell-style, so quotes group |
| `ORCHESTRATOR_WORKDIR` | `<data dir>/orchestrator` | orchestrator cwd; point at the repo checkout (with `--dangerously-skip-permissions`) to run it as a full dev agent |
| `ORCHESTRATOR_TIMEOUT_SECS` | 900 | budget per orchestrator tick. The per-command ceiling is derived as half of it |
| `ORCHESTRATOR_TARGET_DIR` | `<data dir>/verify-target` | `CARGO_TARGET_DIR` for the orchestrator's own verification, set on that child and nowhere else. Shared and long-lived — the warmth is the value. `make verify-warm` primes it |
| `ORCHESTRATOR_TARGET_BUDGET_GB` | 20 | ceiling on that directory, past which the orchestrator loop reclaims it in two tiers. `0` keeps the report and drops the reclaim |
| `WORKER_CMD` | `claude --print …`, no `curl`, no push | worker agent command. The default's omissions are the enforcement: an unattributed local process writes as the *human*, so API access would be a route around the charter |
| `WORKER_TIMEOUT_SECS` | 3600 | budget per worker run — four orchestrator turns, so a suite run need not fit inside the turn a human is waiting behind |
| `TASKS_UPDATE_HOLD` | `on` | whether new scouts and builds wait while an update is pending. `off` keeps the `/status` report and drops the gate; anything else refuses to boot |

Every run budget above is measured on **two clocks** — monotonic and
wall-clock — so a host that sleeps through a budget is a measured fact rather
than a timeout. `crates/tasks/src/deadline.rs` is the whole of that reasoning;
`caffeinate -s` is the operational answer.

## Pool capacity

There are **two ledgers, and they are not the same one.** The *slot* ledger is
`VM_POOL_MAX_VMS` (default 6): this server asks for `SCOUT_MAX_CONCURRENT`
slots for scouts plus exactly **one** for the serial build lane. The *memory*
ledger is the one that bites a small machine first, and `buildkit` is on it
while it is not on the slot ledger: at the default VM shapes, scouts plus a
Builder plus buildkit reserve ≈22 GB.

So the recommended ceiling against the default pool is **`SCOUT_MAX_CONCURRENT
= 3`** — 4 of 6 slots, two spare. At 4 scouts it is 5 of 6 and at 5 it is 6 of
6, where a single leaked VM exhausts the pool and dispatch waits. To go higher,
raise `VM_POOL_MAX_VMS`, restart the *pool*, and check the memory ledger first.

The server reports this arithmetic on every vm-pool connect: too small, or an
exact fit with no slack, is a `warn!` naming the variable and the fix.

## Restarting, draining and stopping

`tasks reload` (alias `restart`) is the upgrade loop the make targets drive:
build, report, gate, drain, swap, verify. It refuses by default when a scout or
a build is in flight — `--when-idle` waits for a drain point, `--force` swaps
anyway. Exit codes: **3** busy, **4** drain timed out, **5** the swap did not
land. `crates/tasks/src/reload.rs` carries the reasoning.

Two asymmetries are worth knowing before you reach for them, because both look
like bugs:

- **A boot does not resume the mode.** It takes `TASKS_DEFAULT_MODE` and
  overwrites the stored one, so a crash or a launchd restart brings the
  pipeline back quiet. `tasks reload` is the one path that carries the mode
  over, and it does so in the child's environment. If you want a host to come
  back dispatching, set `TASKS_DEFAULT_MODE=play` there.
- **`tasks stop --when-idle` leaves dispatch paused**, and plain `tasks stop`
  does not. The only slot in which a stop could put the mode back is *before*
  the SIGTERM, and unpausing a server that is still running hands the
  dispatcher a window to launch one last scout — the unattended VM the flag
  exists to prevent.

`tasks drain` / `tasks resume` are narrower: they quiesce the pipeline and
**hold** it, for the one host act with no recovery — restarting vm-pool on the
same socket. `tasks hold [--label TEXT] -- <command>` is the general wrapper
(pause, run the command as a child, restore the mode on its exit, however it
exits); `make images` is already wrapped in it.

A restart does not cost the work in flight: scouts and builds run under their
own supervisors inside VMs that vm-pool keeps alive, so the only thing a
restart loses is the event stream, and boot re-attaches. **vm-pool is upgraded
separately, and it goes first** — it is a long-lived daemon a server restart
does not restart, so a freshly built server routinely talks to the binary
vm-pool was started with.

## Tests

```sh
make test        # prebuild + cargo-nextest (default profile) + doctests
make test-ci     # same, --profile ci: no fail-fast, retries, quieter slow threshold
make test-cargo  # plain `cargo test --workspace`, no prerequisites
```

`make test` needs `cargo install cargo-nextest --locked`; `make test-cargo` is
the fallback if you don't have it, and is also what keeps the build-on-demand
path in `workspace_bin` honest. Both nextest targets prebuild the supervisor
binaries and export `TASKS_TEST_BIN_DIR` so no test shells out to cargo.

Two gotchas. **nextest does not run doctests** — silently, with no skip count
in its summary — so both targets end with `cargo test --doc --workspace`;
anything else that runs the suite must too. And a handful of tests leave a
stray child holding the output pipe and report as LEAK; that is expected, and
the known set is listed in `.config/nextest.toml` beside the setting that
justifies it. One list, in one place, naming the tests rather than counting
them, so a test leaking for a different reason reads as new.

`app-gpui` is not a workspace member, so none of the above touches it:

```sh
make app-check   # cargo check --all-targets, ~1 minute cold
make app-test    # the app's own unit tests
```

Neither needs a display or a Mac. What does need a Mac is *running* it — a
green `make app-test` says the code compiles and its logic holds, not that a
pixel landed anywhere.
