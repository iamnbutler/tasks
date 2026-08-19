# Tasks

An autonomous software pipeline with a human in the loop. Tasks turns a
GitHub issue tracker into shipped pull requests by orchestrating headless
[Claude Code](https://claude.com/claude-code) agents — and keeps a human in
the position of *reviewer*, not operator.

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

Requires macOS with [apple/container](https://github.com/apple/container),
Rust (edition 2024), and a GitHub token.

```sh
make images                            # build the Scout/Builder VM images (once, and after supervisor changes)
tasks secrets init                     # sealed credential store; unseal key goes to the Keychain
                                       #   (--key-file PATH is first-class, not a fallback:
                                       #    an unsigned dev build is a new application to a
                                       #    macOS access list on every rebuild)
tasks secrets set github-token         # paste the token, ctrl-D (same for anthropic-api-key)
tasks vm-pool &                        # the VM pool, a separate long-lived daemon
make serve                             # the server, in this terminal
cargo run -p tasks -- add-project owner/repo
tasks doctor                           # ...and check the lot: every precondition for a
                                       #   scout, as a checklist, exit 1 on any failure
```

`tasks doctor` is the answer to "can this machine actually run a scout?" — it
asks the container CLI, vm-pool's socket and both its ledgers, the images, the
sealed store, the credential broker VMs redeem leases against, GitHub's answer
to your token, and the rest, in the order the preconditions bite, and names the
command that fixes each failure. It reports and never fixes, and it writes
nothing.

Keys never reach a VM: agents run on short-lived, repo-bound leases redeemed
through an in-process broker, and the sealed store means no raw key sits in
`.env` either (raw `GITHUB_TOKEN` / `ANTHROPIC_API_KEY` env vars still work
as fallbacks). See `docs/plans/2026-08-18-credential-custody.md`.

The server boots **paused**: intake and the API run, nothing dispatches.
Flip to `play` (from the app, or `POST /mode`) and the pipeline starts
pulling queued work. Bulk intake never auto-dispatches — adding a repo with
10,000 issues creates 10,000 backlog rows and zero VM runs; only explicitly
queued tasks reach a Scout.

Day to day:

```sh
make restart      # upgrade a running server in place (drain, swap, verify)
make status
tasks doctor      # preflight: every precondition for a scout, with its fix
make test         # ~565 tests, real processes and real SQLite, no mocks
```

The app (`app-gpui`, built separately — `cd app-gpui && cargo run`) is where
the loop closes: watch a Scout's live Claude Code stream, read its spec,
approve it, and talk to the orchestrator about why it did what it did.

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
