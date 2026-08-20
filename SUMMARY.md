# The README's quickstart cannot be followed end to end (#994)

`README.md`'s "Running it" was a summary of a quickstart rather than one.
Following it literally left a reader with `tasks: command not found` (nothing
put a binary on `PATH`), no Anthropic credential, a `make images` that fails on
unstated toolchain prerequisites and gives no sign it has not hung, no warning
that two of the commands block, and — after every line had succeeded — a
pipeline that dispatches nothing, because a fresh server always boots `pause`
and ingested issues land in `backlog`, which is never dispatched from. The old
text said both of those last two facts, in two separate paragraphs, and never
joined them into "so here is what you type next".

This replaces that section with **Prerequisites / Setup / First run / Day to
day / When something is wrong**. Prerequisites splits what you need to build
and test (Rust, `cargo-nextest`; works on Linux) from what you need to actually
run the pipeline (macOS, apple/container *and* `container system start`, the
cross toolchain, a GitHub token, an Anthropic credential), and says exactly
what `make check-toolchain` does and does not cover. The Anthropic credential
gets its own bullet stating the failure mode in the words that make it
recognisable: nothing warns at boot, the server comes up looking healthy, and
every scout then dies inside its VM on agent auth with a 502 from the broker
reading `the host has no anthropic key configured`. Setup opens with
`cargo build -p tasks` and `export PATH="$PWD/target/debug:$PATH"`, which is
what makes every later `tasks …` line work at all, and explains that
`target/debug/tasks` is literally what `make serve` and `make restart` run
(`TASKS_BIN` in the `Makefile`), so there is one binary and nothing to drift.
The `make images` paragraph names the work rather than inventing a duration —
three release cross-compiles plus four sequenced `container build`s plus the
`--version` read-back — and notes that running it before anything is serving is
safe, because its `tasks drain --check` gate passes when nothing is serving.
"First run" is a new section that closes the gap the old document stopped in
front of: three terminals, which two block, then wait one poll, `GET /tasks`,
`POST /tasks/{id}/queue`, `POST /mode {"mode":"play"}`, with the two
independent reasons that step is not optional. Documentation only — no code, no
tests, no migrations.

Two of the issue's own premises were stale and were corrected rather than
transcribed. The issue's quoted four-line block predates the sealed credential
store, and its suggested disclaimer says the agent VMs "hold a token that
pushes", which has been false since `Scopes::AGENT` became `anthropic` +
`git-read` — the push credential exists only as the server's own ~10-minute
`land` lease, minted on loopback.

## Review feedback

- **1. Do not write a second risk disclaimer; #984's `## Read this first` is
  already in this file.** Confirmed present in the tree I cloned —
  `README.md:8`–`47`, from `e4d11a9` — so I wrote no disclaimer of my own. The
  quickstart opens with a one-line pointer up to it instead. Two things from my
  version were genuinely absent from that section, and per the feedback I added
  them *to it*, in its register, rather than opening a competing one: the nine
  charter capabilities ship not just `live` but **uncapped** (no daily limit,
  no pre-approval gate — `0016_charter_live.sql` struck the caps `0015`
  shipped), and `POST /charter/land_builds {"level":"off"}` as the kill switch
  for someone who wants to merge by hand at first. That is the only edit
  outside the quickstart region. I checked the wire shape rather than assuming
  it: `SetCharter` takes `level`, not `mode`.
- **2. "There is no `tasks doctor` yet" will be false — build against the tree
  you clone.** It is there: `crates/tasks/src/doctor.rs` (2103 lines), from
  `87f4568`, with the subcommand, `--strict` and `--probe-images`. So the
  preface is gone, that sentence is nowhere in the file, and `tasks doctor` is
  the first row of the troubleshooting table. It also shrank the table more
  than the spec anticipated: doctor already answers container services, the
  toolchain, custody, the broker, GitHub's answer to the token, mode and the
  three dispatch holds, so the remaining rows are only the things it genuinely
  cannot see — a task sitting in `backlog` (per-task state), which hold is in
  force, image staleness, and a scout that started and died.
- **3. Your line range is the region three other builds are editing — rebase
  and say where each line ended up.** Done; the map is in *Lines found and
  where they went* below. Nothing was dropped. #983's `## License` sits below
  the region and is untouched.

## Directions

- **Dispatched last, behind six builds, four of which edit `README.md`;
  account for all three required changes.** Done, above.
- **1. `## Read this first` — point to it, don't duplicate it.** Found present;
  one-line pointer, plus the two additions named above folded into that
  section.
- **2. `tasks doctor` — if it is in the tree it is the first row and the
  preface goes.** Found present; preface gone, first row.
- **3. `cp .env.example .env` (#989) and the `tasks doctor` line (#990) land
  inside the region — carry them forward.** Both found and both carried, along
  with everything else in the block. Details below.
- **Report this as a rewrite of a contested region, not an additive edit.**
  That is what the section below is.

### Lines found and where they went

The region I replaced was `README.md:101`–`151` in the tree I cloned (the spec
said 60–214, written against an older file). Every line in it:

| found at | what it was | where it is now |
| --- | --- | --- |
| 103–104 | "Requires macOS with apple/container, Rust, and a GitHub token" | expanded into `### Prerequisites`, split build-and-test from run-the-pipeline |
| 107 | `make images` | `### Setup`, with a new paragraph on what it does and how long it is silent |
| 108–111 | `tasks secrets init` + the `--key-file` comment | `### Setup`, verbatim |
| 112 | `tasks secrets set github-token` (`same for anthropic-api-key`) | `### Setup`, split into two lines with `anthropic-api-key` first — which is what `tasks secrets init` itself prints as the next step |
| 113 | `cp .env.example .env` (#989) | `### Setup`, verbatim |
| 114 | `tasks vm-pool &` | `### First run`, terminal 1, without the `&` since it now has its own terminal |
| 115 | `make serve` | `### First run`, terminal 2 |
| 116 | `cargo run -p tasks -- add-project owner/repo` | `### Setup` |
| 117–118 | `tasks doctor` in the block (#990) | `### First run` terminal 3, and again in `### Day to day` |
| 121–126 | the `tasks doctor` paragraph (#990) | `### Day to day`, intact |
| 128–132 | the credentials / broker paragraph (#989) | end of `### Setup`, with "warned at startup" added about the env fallbacks |
| 134–138 | "boots paused" + "bulk intake never auto-dispatches" | `### First run`, as the two bullets explaining why the queue and mode calls are both required |
| 140–147 | the Day-to-day block | `### Day to day`, plus the `make drain` → `make images` → `make resume` triple |
| 146 | `make test  # ~565 tests` | kept, **number removed** — the count is stale and replacing one stale number with another is the same bug; the line now describes the suite's character |
| 149–151 | the `app-gpui` paragraph | end of `### First run`, where the loop it describes actually closes |

## Notes

- Every factual claim was checked against the source rather than carried from
  the spec: the charter and mode wire shapes (`SetCharter.level`,
  `SetMode.mode`) and the routes in `server.rs`; `list_active_tasks`, which
  hides only rows that are both `gh_state = closed` *and* terminal, so freshly
  ingested open backlog rows do appear on `GET /tasks` without `?all=true`;
  the `make images` dependency chain in the `Makefile`; `check-toolchain`'s
  three checks; the broker's 502 text; `autospawn_enabled`, which is `off` for
  a checkout artifact, so `tasks vm-pool` really does have to be started by
  hand here; and the defaults (port 4800, `TASKS_POLL_INTERVAL` 60).
- One claim I strengthened because the code was more specific than the spec:
  `tasks service install` versus `make serve` is not a vague conflict —
  `reload.rs` refuses `--foreground` against a launchd-managed running server
  with "would race the service for the port", and the README now says that.
- One correction the spec did not anticipate, in its own commit. The first
  draft of "First run" told the reader to wait a poll interval before looking
  for tasks. `poll_loop` polls and *then* sleeps, and only `Mode::Stop` skips
  the poll — so on a server that boots `pause`, intake has already happened by
  the time it answers. The text now says that, and keeps the interval where it
  actually applies: a project added after the server is up.
- Out of scope and left alone, as the spec flagged: `CLAUDE.md` still carries
  "~565 tests in ~21s" in the warm-build-directory rule. For the record the
  suite is 963 nextest tests in ~40s here, which is why the README now
  describes the suite's character and states no count at all — replacing one
  stale number with another is the same bug one iteration later.

Verification: PASSED — `cargo fmt --all --check`, `cargo clippy --workspace --all-targets` (both clean), and `make test` (963 tests run, 963 passed, 0 failed, doctests included, exit 0)
