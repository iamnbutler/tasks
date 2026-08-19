# `tasks doctor`: one command that says whether this machine can run a scout

A new `tasks doctor` subcommand asks every precondition for a scout at once and
prints a checklist in the order the preconditions bite — `.env` and the data
dir, whether the configuration parses at all, the container CLI and its system
services, the toolchain `make images` needs, vm-pool's socket / protocol / slot
ledger / memory ledger, the server and its build and mode and dispatch holds,
the two VM images, credential custody (sealed store, unseal key, and *which
source* answers for each of the two keys), the credential broker VMs redeem
their leases against, the GitHub token's identity and scopes read live from the
API, whether any project is tracked, and the orchestrator's command, workdir and
build directory. The ordering is load-bearing rather than decorative: a missing
container CLI explains the vm-pool failure below it, which explains the dispatch
failure below that, so nothing is sorted by severity.

It reports and never fixes. Every failing check carries the command that changes
it, and that is enforced by `Check::fail`'s and `Check::warn`'s signatures rather
than by convention — a required parameter cannot be forgotten, because it does
not compile; `Check::note` is the *named* escape hatch for the warnings with
genuinely no command, so "there is nothing to run" cannot be mistaken for
"somebody forgot to write the fix down". Nothing short-circuits: a question that
could not be *made* is a `Skip` naming its reason, so the report has the same
shape on a broken machine as on a working one, and a `Skip` never sets the exit
code (every skip has a failure above it that caused it, and reporting one broken
thing as two would make `--strict` unreadable on a host with no container CLI).
It writes nothing — not to GitHub, not to the store, not to a VM — with one
stated exception, the data-dir write probe, which creates and removes a single
uniquely-named file because writability is only answerable by writing; a test
asserts the directory is as it found it, and an integration test asserts a bare
data dir is still empty afterwards. In particular it **never opens the store**,
because `Store::open` runs migrations, so mode, projects and the observed image
identities come from the running server's HTTP API and a host with no server
reports "not serving" rather than reaching past it. It never prints a
credential, only which source answered, which is structural: the type it prints
has no value in it. Exit is `0` clean, `1` on a failure (or on any warning under
`--strict`), `2` on a usage error.

Facts are reused rather than restated, which was the spec's best instinct:
`run::Capacity` for the slot ledger, `reattach::support_of` for the protocol
(it already names its own fix, so it is rendered verbatim),
`ImageFreshness::needs_rebuild` for image staleness, `env_file::Source` for what
each `.env` contributed (names only, never values), `secrets::status` for the
sealed store — which deliberately works when the unseal key is what is missing —
and `reload::workspace_above` / `fetch_status` for the checkout probe and the
server. Four small enabling changes elsewhere: `secrets::CredentialSource` +
`Secrets::source_of` / `Secrets::unresolvable`; `github::Viewer` +
`GitHubClient::viewer()`; `run::memory_reserve_mb` and `BUILDKIT_RESERVE_MB`;
and a `Config::from_env` split so custody can be asked about before the config
is built. 35 new tests (24 in `doctor`, 3 in `secrets`, 5 in `github`, 1 in
`run`, 3 integration tests against the real binary in `tests/cli.rs`, which
already sets `TASKS_ENV_FILES=off` as CLAUDE.md requires of any file that execs
it).

## Review feedback

- **1. The credential broker was missing — added as a section, not a tweak.**
  Done, and it is the check that justifies the command. `probe_broker` connects
  to `TASKS_BROKER_ADVERTISE:TASKS_BROKER_PORT` — **never loopback** — and an
  unauthenticated `401` is the success condition, which the check's `detail`
  says out loud because it reads backwards. It is written at the TCP level
  rather than through `reqwest` precisely so the three failures stay apart:
  refused, unreachable, and *accepted-then-produced-no-HTTP*. One deliberate
  refinement of the feedback's wording, found by writing the test against a real
  listener: a severed listener does not always return zero bytes — it can reset
  the connection, which arrives as an I/O error. So **everything that fails
  after a successful connect is `Silent` and a `Fail`**, never `Unreachable`;
  the connect already proved the address is reachable, and letting a reset
  demote the finding to a `Skip` would set no exit code, which is the exact
  false negative the check exists to prevent. The honest limit is stated rather
  than oversold: an address that cannot be reached at all is a `Skip` naming the
  bridge gateway that does not exist until the first container has started.
  (Incidentally verified in the field — run from this Builder VM, the probe read
  the host's real broker at `192.168.64.1:4801` and got the 401.)
- **2. `Capacity` classified in one place and its severity decided in two.**
  Fixed at the source: `Capacity::level()` is the one mapping, and
  `Capacity::describe()` and `Capacity::fix()` join it so the sentence and the
  command are single-sourced too. `report_capacity` now *reads* all three
  instead of deciding them, and the doc-comment argument for why `NoSlack` is a
  warning has moved onto `level()` where both callers can see it. A test pins
  the three levels and that both bad ones carry a fix. For `ImageFreshness`,
  doctor reads the existing `needs_rebuild()` predicate rather than mapping the
  enum a second time, so there was no second decision to remove.
- **3. Verify `x-oauth-scopes` on GraphQL, and say that you did.** Checked, and
  the premise does **not** hold up, so the design changed. Evidence: GitHub
  documents the header for REST and nowhere for GraphQL; `gh auth status` reads
  it off a **REST** call (`cli/cli` PR #6546 — `GetScopes` builds its request
  from `ghinstance.RESTPrefix`, and its test mocks `httpmock.REST`); and I could
  not test an authenticated GraphQL response from here, having no GitHub token
  (this VM holds only a repo-bound broker lease). The one supporting datum is
  that an unauthenticated GraphQL response *does* send
  `access-control-expose-headers: …, X-OAuth-Scopes, …` — suggestive, and not
  the same as observing the header. So `viewer()` uses the GraphQL header when
  it is present and never relies on it: absent, it falls back to
  `GET /rate_limit`, which is documented to carry the header and to not consume
  rate-limit quota. `Viewer::scope_source` records which response answered, so a
  future reader can confirm the premise empirically on a real machine instead of
  re-deriving it, and "not enumerable" is only ever reported after both were
  asked. `None` vs `Some(vec![])` stays distinguishable the whole way to the
  renderer, with a test pinning both.
- **Open question — the API-surface cost.** Taken seriously; **everything is
  `pub(crate)`, not `pub`**. `Capacity`, `Capacity::assess` (and the new
  `level`/`describe`/`fix`/`needed`/`total`), `BUILD_LANE_SLOTS`,
  `BUILDKIT_RESERVE_MB`, `memory_reserve_mb` and `Config::from_env_with` are all
  `pub(crate)` — doctor is the only caller and it lives in this crate. The same
  narrowing applied to the two additions the spec proposed as `pub`:
  `secrets::CredentialSource` / `source_of` / `unresolvable` and
  `github::Viewer` / `ScopeSource` / `viewer()` are `pub(crate)` too (the `tasks`
  binary is a separate crate and reaches none of them; `viewer()` is one word
  from `pub` whenever the identity issue wants it). The only genuinely `pub`
  surface added is `doctor::{Level, Check, Section, Report, DoctorOptions, run}`
  — `main.rs` needs the last three, `run.rs` reads `Level`, and the spec's
  downstream note asks that these stay plain data so the move into `tasks-api`
  for the app's `--json` mode is mechanical.
- **Sequencing: build after #1003 and rebase onto it.** Done — #1003 had landed
  (`e19d256`) before this branch was cut, and `source_of` is written against the
  code actually there. See the directions below for what that shape turned out
  to be.

## Directions

- **Account for the review feedback, the broker first.** Done, above.
- **#1003 lands first; implement `source_of` against the code you find.** The
  shape I found: the resolution order is unchanged (sealed store live → the
  boot-captured process environment → for the Anthropic key only, the host's
  `~/.claude/anthropic_key.sh`), but the custody boundary is now the `keyring`
  crate's native backends with the `/usr/bin/security` read as the default
  fallback, and `key_location`/`KeyLocation` is the single decision both
  `resolve_unseal_key` and `status` read. So `source_of` needed no adaptation of
  its *order* — it is `get` with the value thrown away and the location kept,
  written as one `match` beside `get`'s body so the two cannot drift — but the
  fallback map had to change shape: `env_fallbacks()` now carries an
  `EnvFallback { value, source }` per name and `anthropic_key_from_host_helper()`
  returns its script's path, so the helper is not re-run at report time (which
  could answer differently from the value actually in use). A test asserts
  sealed outranks the environment in `get` *and* in `source_of` alike.
- **README.md / CLAUDE.md are contended; insert, leave neighbours
  byte-identical, run no formatter.** Done. README gained two lines in the
  setup block plus a short paragraph, and one line in "day to day". CLAUDE.md
  gained one rule bullet immediately before `## Project structure` and two lines
  in the *Running* block. No formatter was run over either file; `git diff
  --stat` shows insertions only (+10 README, +56 CLAUDE.md).
- **Take the narrowest visibility that works, and report per item.** Done —
  itemised above.
- **Confirm `container system status` and `container images list` against the
  real CLI** (the spec's own pitfall). The spec's spelling was **wrong** and is
  corrected: apple/container's command reference documents `container image
  list` — **singular**, alias `ls` — under Image Management, with no `images`
  alias anywhere. `container system status` is right ("Checks whether the
  container services are running"). Both are used as documented, and both paths
  still degrade honestly, so a spelling that goes stale on a future version
  produces a wrong *message* and never a false pass. `lists_image` remains the
  single function to change if the column layout differs; it matches NAME and
  TAG as two fields *or* a single `name:tag` token, and deliberately never a
  substring — a test pins that a row for `myagent v1` does not answer for
  `agent:v1`.
- **Bias toward checking one more thing; ground it in what fails on a fresh
  machine; `CARGO_TARGET_DIR` / disk space; no writes; every failure names its
  fix; state what the exit code means.** All carried over from the spec and
  honoured; the toolchain section is `Warn`-only (a host whose images are built
  runs scouts perfectly well without any of it), the cross linker is asked about
  only where the image triple differs from the host's, and the verify target dir
  is a warning because it is not a scout precondition.

Two things the spec asked for that are deliberately **not** here, both as the
spec itself proposed: no `--fix` flag, and no `--json` mode or move of the
report types into `tasks-api` (phase 2 of the distribution plan) — the types are
kept plain-data with no server references so that move stays mechanical. One
finding is surfaced and not fixed, as the spec instructs: the orchestrator
prompt derives `workdir_is_checkout` from `ORCHESTRATOR_WORKDIR` merely being
set, so pointing it at any directory promises the agent a repository it does not
have. Doctor uses the real probe and warns about the discrepancy explicitly;
fixing the prompt generator is a separate change.

One number needed calibrating rather than transcribing: `BUILDKIT_RESERVE_MB` is
2048, back-solved from CLAUDE.md's own ≈22 GB *Pool capacity* arithmetic at the
default shapes (2 scouts × 6144 MB + a Builder at 8192 MB), and a test pins the
sum so changing either default without revisiting that sentence goes red here.

Verification: PASSED — `make test` (934 tests, 0 failed — the 7 LEAKs are the
documented expected ones — plus `cargo test --doc --workspace`), with
`cargo clippy --workspace --all-targets` clean and `cargo fmt --all` applied.
