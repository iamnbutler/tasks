# End-user distribution: the shape of Tasks off a checkout

*2026-08-18*

## The problem, precisely

Every runnable artifact this system depends on lives today in `target/` of a
developer checkout: the serving `tasks` binary, the vm-pool daemon (the same
binary), and the toolchain the app's Restart menu invokes (`tasks reload` runs
`cargo build` as its first act). That has two failure modes, and we just paid
for the first one:

1. **`cargo clean` deletes the running system out from under itself.** The
   pidfile's `exe` points at `target/debug/tasks`; the app's binary resolution
   already knows a pidfile can outlive its binary (`resolve_binary` checks
   `is_file`), but knowing the binary is gone is not the same as having one.
   After a clean there is nothing runnable anywhere except by rebuilding, and
   the thing that rebuilds (`tasks reload`) was itself deleted.

2. **An end user who downloads Tasks.app has no `target/` at all.** No cargo,
   no checkout, no cross toolchain, no `container build`. Every flow that
   assumes one — the reload build step, `make images`, `tasks vm-pool` typed
   into a terminal — is a dead end for them.

Both are the same defect: the system's stable home is a build cache. The fix
is one shape that serves both audiences, not an end-user mode bolted beside
the dev mode.

## The shape

**Tasks.app is the distribution.** One bundle carries both executables:

```
Tasks.app/
  Contents/
    Info.plist                # version = build stamp, same scheme as today
    MacOS/
      Tasks                   # the gpui app
    Helpers/
      tasks                   # the server CLI, release build
```

The server binary rides in `Contents/Helpers`, **not** beside the app binary
in `Contents/MacOS` — the first dist build tried that and the `cp` silently
*overwrote the app*: the default macOS filesystem is case-insensitive, so
`MacOS/tasks` and `MacOS/Tasks` are the same directory entry, and a sibling
probe for `tasks` from the app resolves to the app itself. `Helpers` is a
standard nested-code location, so codesigning (phase 2) needs no exception
for it. Data stays in `~/.local/state/tasks-v2` (`TASKS_DATA_DIR`); the
bundle stays stateless, so replacing it is always safe.

The process model does not change. The same two long-lived processes run on
an end user's machine that run on a dev machine — `tasks serve` and `tasks
vm-pool` — with the same lifetimes (the pool outlives the server; both
outlive the app, which is what makes the pipeline autonomy-forward rather
than a window you have to keep open). What changes is only **who starts them
and where the binary comes from**:

| | dev checkout | end user |
| --- | --- | --- |
| `tasks` binary | `target/debug/tasks` via `make serve` | `Tasks.app/Contents/Helpers/tasks` |
| server start | terminal (`make serve`/`restart`) | the app, via `tasks reload --no-build` |
| server upgrade | `tasks reload` (builds) | replace the bundle; `reload --no-build` swaps it in |
| vm-pool start | terminal (`tasks vm-pool`) | the server spawns it (autospawn, below) |
| VM images | `make images` (cross-compile + `container build`) | pulled from a registry (phase 2) |
| apple/container CLI | brew'd by the dev | first-run check, guided install (phase 2) |

### Starting the server: the app, driving `--no-build`

The app already knows how to start and swap a server — the Server menu runs
`tasks reload`, and a reload with nothing serving *is* a start. Two additions
make that work off a bundle:

- **Binary resolution learns the bundle.** `resolve_binary` gains one source:
  the bundle's `Contents/Helpers/tasks`. Order: `$TASKS_BIN` (explicit
  always wins) → the pidfile's `exe` if it still exists (the binary that *is*
  serving is the most obviously correct thing to restart) → **the bundled
  binary** → `$PATH`. A dev app built by `make app` has no bundled `tasks`,
  so dev behaviour is byte-identical; an installed bundle finds its own
  server without a `PATH` guess. On update the install path is stable
  (`~/Applications/Tasks.app/Contents/Helpers/tasks`), so a pidfile written
  by the old install resolves to the *new* binary at the same path — which is
  exactly the binary a restart should pick up.

- **Ops on a bundled binary pass `--no-build`.** `reload --no-build` swaps in
  `current_exe()` — the binary running the reload — which for a bundled
  install is precisely the update: the new bundle's binary swaps itself in,
  `ModeHandover` carries the mode, and the existing `/status`-on-new-pid
  verification answers "did it take". The flag is derived from where the
  binary was found, never a user setting: a binary inside an app bundle has
  no workspace to build, and asking cargo to prove that on every restart is
  how an end user meets a compiler error dialog.

Updates are then just: replace the bundle (download, drag, or a phase-2
updater), and the app — which already compares `/version` of the serving
process against its own build stamp — offers Restart. `UpdateWatch`'s
binary-freshness hold works unchanged: the bundle path is stable, so the new
install is "a newer binary at `current_exe`'s path" exactly as a rebuilt
`target/debug/tasks` is.

### Starting vm-pool: the server spawns it

An end user will never type `tasks vm-pool` into a terminal, and the pool
must not become an in-process component of the server — its whole value is
that it survives server restarts, which is what makes `resume_in_flight`
possible. So the pool stays a separate daemon, and the server becomes able to
**spawn** it: when the scout dispatch loop finds the socket unreachable and
`TASKS_VM_POOL_AUTOSPAWN=on`, it spawns `current_exe() vm-pool` detached
(own process group, stdio to `<data dir>/vm-pool.log`) and retries the
connect on its normal cadence.

What makes this safe is a guard that already exists: **the pool refuses to
start when something is listening on its socket.** Autospawn therefore needs
no leader election and no pidfile — if two spawns race, or a human starts
their own pool in the same window, exactly one binds and the rest exit,
logged, having touched nothing. The spawned pool inherits the server's
environment, so `VM_POOL_SOCKET` and `VM_POOL_MAX_VMS` mean what they always
meant, and the orphan-ledger recovery (successor stops predecessor's
containers) applies to autospawned pools the same as hand-started ones.

Strictly parsed (`on`/`off`, anything else refuses to boot — the
`TASKS_UPDATE_HOLD` convention), and **unset means derived from where the
binary lives**, by the same probe the app's `--no-build` decision uses (is
there a `crates/tasks/Cargo.toml` above `current_exe()`): an installed
binary manages its own pool, a checkout artifact defaults off. Off for
checkouts because there the pool is a thing the developer deliberately
restarts (`make drain` → restart pool → `make resume`), and a server
helpfully respawning the *old* binary's pool in the gap races the
developer's own upgrade — the refusal guard makes that race safe, not
polite. Deriving in the server rather than having the app inject
`TASKS_VM_POOL_AUTOSPAWN=on` for bundled binaries keeps the precedence
honest: an env var set by the app would silently outrank the user's own
`<data dir>/.env`, which is exactly the file a power user would set `off`
in.

Restarting an autospawned pool (phase 2, if wanted) is the existing drain
story: `tasks drain`, kill the pool, and the server's next failed connect
respawns it from the current binary — which after a bundle update is the new
one. Note the ordering property this buys for free: today vm-pool "goes
first" in an upgrade by operator discipline; with autospawn the pool is
always respawned from the binary that is serving, so pool-newer-than-server
skew cannot arise from an update, only from a not-yet-restarted pool — which
`dispatch_loop` already logs on every connect.

### Images: pulled, never built (phase 2)

The images cannot be built on an end user's machine and should not be: the
cross toolchain and `container build` are release-time work. The release
pipeline publishes them —

```
make images-push        # -> ghcr.io/iamnbutler/tasks-scout:0.1.<n>
                        #    ghcr.io/iamnbutler/tasks-builder:0.1.<n>
```

— tagged with the same build stamp the binaries carry, and the end-user
machine fetches with `tasks images pull [--version]`, which shells to
`container image pull` and then tags locally as `agent:v1` / `builder:v1` so
`SCOUT_IMAGE`/`BUILDER_IMAGE` defaults keep meaning what they mean. The
existing image-identity machinery (`images.rs`, `Started`-event stamping,
`UpdateWatch`) then reports stale/current exactly as it does for hand-built
images; "PREDATES STAMPING" and `make images-check`'s window both apply
unchanged. Pulling rather than bundling because the images are multi-GB (they
carry a Rust toolchain), a registry pull is resumable, and the bundle must
stay a quick download.

### Host dependencies and first run (phase 2)

The one dependency that cannot ride in the bundle is **apple/container** —
it is a system service with its own installer. First run therefore needs a
setup surface. The CLI half is `tasks doctor`: report each precondition and
its fix — container CLI present, `container system` started, images present,
secrets sealed (`tasks secrets init`/`set` already work on a bare Mac via
Keychain), server serving, pool answering. The app half is a Setup window
that renders the same checklist and drives the fixable ones (secrets entry
piping to `tasks secrets set` stdin; an images pull with progress). The
`GITHUB_TOKEN`-less degraded mode the server already has (polling disabled,
API up) is what makes the app usable *before* setup completes.

### What deliberately does not change

- **The data dir.** `~/.local/state/tasks-v2` for both audiences. Moving end
  users to `~/Library/Application Support` would fork every path in the docs
  for a cosmetic win, and the `.env`-in-data-dir mechanism is already the
  launcher-independent config home an installed binary needs.
- **launchd.** Not in phase 1. The app starts what is missing, and both
  daemons are detached, so closing the app strands nothing. What launchd adds
  is start-at-login and restart-on-crash; both are additive later (point a
  LaunchAgent at the bundled `tasks serve` — the boot-comes-up-paused rule
  was designed for exactly that supervisor), and neither is needed to make
  the download-and-run story true.
- **The orchestrator's degraded mode.** On a machine with no checkout,
  `workdir_is_checkout` is false, `can_verify` is false, and the generated
  prompt sections already say so honestly. Verification-by-own-run is a dev
  -machine capability; carve-out (b) routes those batches to a human, which
  for an end user is the review UI they are already in.
- **Dev flows.** `make serve`/`restart`/`images` all keep working against
  the checkout. The one recommended habit change: run your daily-driver
  system from an installed bundle (`make dist` below), so a `cargo clean` in
  the checkout can never again take the serving binary with it.

## Phase 1 (this branch)

1. **`make dist`** — assemble the self-contained bundle: `app-build` +
   `cargo build --release -p tasks` (stamped like every other installed
   artifact) + install with the server binary at `Contents/Helpers/tasks`.
   `make app` stays the dev bundle (no embedded server, no release build tax
   on the inner loop).
2. **App: bundled resolution + derived `--no-build`** in
   `app-gpui/src/server.rs`, with the resolution-order and no-build tests
   beside the existing ones.
3. **Server: `TASKS_VM_POOL_AUTOSPAWN`** — config parse (strict), spawn in
   the dispatch loop's failed-connect arm, `vm-pool.log`, and a real-process
   test: serve with autospawn on and a tempdir socket; assert the socket
   comes up and answers; kill both.
4. This document.

## Phase 2 (named, not started)

- `make images-push` / `tasks images pull` (registry: ghcr.io), and the
  release workflow that publishes bundle + images together under one stamp.
- `tasks doctor`; app Setup window (container CLI check, secrets entry,
  images pull).
- Codesigning + notarization (unsigned bundles get quarantined on download —
  until then the README documents `xattr -d com.apple.quarantine`).
- A DMG (or zip) `make release` artifact; update check against GitHub
  releases in the app's About window.
- Optional launchd LaunchAgents ("Start at login") installed by the app.
