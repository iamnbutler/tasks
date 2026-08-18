# End-user distribution: the daemon is the product

*2026-08-18. Revised the same day: the first draft made the app the server's
home and lifecycle owner; review pulled it to the OrbStack/Tailscale shape —
a launchd service with clients, the app being one of them.*

## The problem, precisely

Every runnable artifact this system depends on lived in `target/` of a
developer checkout: the serving `tasks` binary, the vm-pool daemon (the same
binary), and the toolchain the app's Restart menu invokes (`tasks reload`
runs `cargo build` as its first act). That has two failure modes, and we
paid for the first one:

1. **`cargo clean` deletes the running system out from under itself.** The
   pidfile's `exe` points at `target/debug/tasks`; after a clean there is
   nothing runnable anywhere except by rebuilding, and the thing that
   rebuilds (`tasks reload`) was itself deleted.

2. **An end user who downloads Tasks.app has no `target/` at all.** No
   cargo, no checkout, no cross toolchain, no `container build`. Every flow
   that assumes one is a dead end for them.

Both are the same defect: the system's stable home was a build cache. And
the first fix attempted here had the same defect one level up — embedding
the server in the app bundle and letting the app drive it makes the *bundle*
the serving binary's home, so deleting or updating the app deletes the
server out from under a live pipeline, and a headless Mac can't run the
system at all. The app is a client. The daemon is the product.

## The shape

**The server is its own installable; launchd owns its lifecycle; every
client is just a client.**

```
~/.tasks/bin/tasks                        # the binary's stable home
~/Library/LaunchAgents/
  com.iamnbutler.tasks.server.plist       # RunAtLoad + KeepAlive, runs
                                          #   `tasks serve`, pins TASKS_DATA_DIR
~/.local/state/tasks-v2/                  # data dir, unchanged
Tasks.app/                                # a client, which also carries a seed
  Contents/MacOS/Tasks                    #   the gpui app
  Contents/Helpers/tasks                  #   the seed copy of the server
```

The seed rides in `Contents/Helpers`, **not** beside the app binary in
`Contents/MacOS` — the first dist build tried that and the `cp` silently
*overwrote the app*: the default macOS filesystem is case-insensitive, so
`MacOS/tasks` and `MacOS/Tasks` are the same directory entry, and a sibling
probe for `tasks` from the app resolves to the app itself. `Helpers` is a
standard nested-code location, so codesigning (phase 2) needs no exception.

The process model does not change. The same two long-lived processes run on
an end user's machine that run on a dev machine — `tasks serve` and `tasks
vm-pool` — with the same lifetimes. What changes is who starts them:

| | dev checkout | end user |
| --- | --- | --- |
| `tasks` binary | `target/debug/tasks` via `make serve` | `~/.tasks/bin/tasks` |
| server lifecycle | terminal (`make serve`/`restart`/`stop`) | launchd (`tasks service …`) |
| vm-pool lifecycle | terminal (`tasks vm-pool`) | the server (autospawn, below) |
| server upgrade | `tasks reload` (builds) | `tasks service install` from a newer binary |
| VM images | `make images` | pulled from a registry (phase 2) |
| apple/container CLI | brew'd by the dev | first-run check, guided install (phase 2) |

### The service: one LaunchAgent, and the server supervises its own pool

`tasks service install` copies **the binary it is run from** to
`~/.tasks/bin/tasks` (write-then-rename; overwriting a running signed
executable in place is how macOS kills the process), writes the LaunchAgent,
loads it, and waits for `/status` on the new pid. It is idempotent and it is
also the upgrade: run it from any newer binary — the app's bundled seed, a
checkout, an installer download — and the service now serves that binary.
`uninstall`, `start`, `stop`, `restart`, `status` are the rest of the verbs;
`stop` is `bootout`, because under `KeepAlive` a plain SIGTERM is a restart.

There is deliberately **no second LaunchAgent for vm-pool**. The pool must
stay a separate daemon (it outliving server restarts is what makes
`resume_in_flight` possible), but its supervisor is the *server*: with
`TASKS_VM_POOL_AUTOSPAWN` on, a failed connect in the dispatch loop spawns
`current_exe vm-pool` detached (own process group, logging to
`<data dir>/vm-pool.log`) and retries on its normal cadence — which is also
restart-on-crash for the pool. No leader election and no pidfile, because
the pool already **refuses to start when something is listening on its
socket**: racing spawns, or a spawn racing a human's own `tasks vm-pool`,
resolve to one bound daemon. The autospawn default is **derived from where
the binary lives** (the same `crates/tasks/Cargo.toml`-above-the-binary
probe reload uses to find a workspace): an installed binary manages its own
pool, a checkout artifact stays out of the developer's way — whose
deliberate pool restarts (`make drain` → restart pool → `make resume`) an
eager server would race. Explicit `on`/`off` beats the derivation; garbage
refuses to boot.

The boot mode stays the quiet default. The plist pins `TASKS_DATA_DIR` and
nothing else unless asked: `--default-mode play` is the explicit opt-in for
"this host comes back dispatching", and it applies to every boot including
crash restarts, which is exactly why it is an opt-in rather than something a
reload writes through the plist.

### reload and stop delegate — that's what keeps one mental model

`tasks reload` and `tasks stop` detect a managed server and route through
launchd instead of SIGTERM: a reload puts its binary in `~/.tasks/bin` and
kickstarts (so `make restart` from a checkout means "make the service serve
this build" — deliberate); a stop unloads the agent. Without the delegation,
`tasks stop` would report "stopped" while `KeepAlive` resurrects the server
behind the report.

The delegation guard is three-fold, and the middle test is the one that
protects everyone else: the plist's pinned `TASKS_DATA_DIR` must equal the
data dir in hand. The service's identity *is* its data dir, so a reload
pointed anywhere else — every test's tempdir, a second deployment — is about
a different server and never touches the operator's real launchd session.
The third test: a pidfile naming any other binary is a developer serving
beside the service, and their `make restart` keeps meaning what it always
meant. Mode carry degrades gracefully under launchd: launchd owns the
child's environment, so the carry is a `POST /mode` after the verify, and
the window between boot and that write runs in the plist's default — quiet,
unless the operator pinned `play`, and the carry names the window when it
happens.

### The app: a client, with a bootstrap button

The app talks HTTP like every other client. Its Server menu ops resolve a
binary (`$TASKS_BIN` → the pidfile's `exe` → the bundle's seed → `$PATH`)
and what they run depends on what was found:

- **The pidfile's binary**: `reload --no-build` / `stop`, which delegate to
  launchd on their own when that binary is the service's. `--no-build` is
  derived from the binary's surroundings (no workspace above it → nothing to
  build), never a setting.
- **The bundle's seed** — nothing serving, nothing installed, the end user's
  first launch: the building ops become **`tasks service install`**. That is
  the one-button install: seed `~/.tasks/bin`, register the agent, start.
  From then on the app is a pure client; deleting Tasks.app changes nothing
  about the running service, which is the decoupling test in one sentence.

Updates travel separately. An app update is a new client plus a newer seed
riding along unused; the app already compares `/version` of the serving
process against its own stamp, so "update the service from this app" is a
Restart click away (`reload --no-build` from the seed → delegation installs
it). A menubar status item is phase 2+; the CLI and the Server window cover
status until then.

### Images: pulled, never built (phase 2)

Unchanged from the first draft: the release pipeline publishes
(`make images-push` → `ghcr.io/iamnbutler/tasks-{scout,builder}:0.1.<n>`,
stamped like the binaries), and `tasks images pull` fetches and tags locally
as `agent:v1` / `builder:v1` so the image-identity machinery (`images.rs`,
`UpdateWatch`) applies unchanged. Pulling rather than bundling because the
images are multi-GB, a registry pull is resumable, and neither the bundle
nor an installer script should be.

### Host dependencies and first run (phase 2)

apple/container cannot ride in any bundle — it is a system service with its
own installer. `tasks doctor` reports each precondition and its fix
(container CLI present, `container system` started, images present, secrets
sealed, service installed/loaded/serving, pool answering); the app renders
the same checklist and drives the fixable ones. The tokenless degraded mode
the server already has is what makes the app usable before setup completes.

### What deliberately does not change

- **The data dir.** `~/.local/state/tasks-v2` for both audiences; the
  `.env`-in-data-dir mechanism is already the launcher-independent config
  home an installed binary needs.
- **The dev flows.** `make serve`/`restart`/`images` work against the
  checkout exactly as before wherever no service is installed — and the
  delegation guard keeps every test and every scratch data dir out of the
  operator's launchd session by construction.
- **The orchestrator's degraded mode.** No checkout → `can_verify` false →
  the generated prompt sections already say so honestly.

## Phase 1 (this branch)

1. `make dist` — the bundle with the seed at `Contents/Helpers/tasks`
   (`make app` stays the dev bundle).
2. App: resolution order with the seed, derived `--no-build`, and seed
   restarts mapping to `service install`.
3. `TASKS_VM_POOL_AUTOSPAWN` with the derived default, spawn in the dispatch
   loop, `vm-pool.log`, real-process test.
4. `tasks service install|uninstall|start|stop|restart|status`; `reload`/
   `stop` delegation with the pinned-data-dir guard.
5. This document.

## Phase 2 (named, not started)

- `make images-push` / `tasks images pull`; a release workflow publishing
  installer + images under one stamp; a curl-able installer script and/or a
  brew tap that lands the same `~/.tasks/bin` + `tasks service install`.
- `tasks doctor`; the app's Setup surface (container CLI check, secrets
  entry, images pull, service state) — including replacing the seed-install
  button's generic run output with a real first-run flow.
- Codesigning + notarization; a DMG/zip `make release` artifact; update
  check in the app.
- A menubar status item (or tiny separate menubar target).
