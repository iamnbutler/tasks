# Credential custody: sealed secrets, short-lived leases, and the broker

Issue #971 asks for the keys to be rotated. Before rotating, this change makes
the rotated keys structurally unleakable in the ways the old ones leaked
(#923/#970): after it, **no VM ever holds a raw `ANTHROPIC_API_KEY` or
`GITHUB_TOKEN` in any form, and the server holds them only in guarded process
memory** — never in `.env`, never in its environment, never in a clone URL it
hands out, never on disk in plaintext.

## The three mechanisms

### 1. Sealed storage on the host (`crates/tasks/src/secrets.rs`)

Raw keys live in one place: `<data dir>/secrets/sealed.json`, encrypted
per-entry with ChaCha20-Poly1305 under a key derived (HKDF-SHA256, per-store
salt, entry name as AAD) from a 32-byte **unseal key** that is *not stored
next to the ciphertext* — that is the "two key" property. The unseal key lives
in the macOS Keychain (`security` CLI, service `tasks-v2-secrets`) or, where a
Keychain is not available (Linux, tests, CI), in a file named by
`TASKS_SECRETS_KEY_FILE`. A copied data dir — a backup, a synced folder, a
bundle of the server's state — yields ciphertext only; a copied Keychain entry
yields a key with nothing to open.

`tasks secrets init | set <name> | status | rm <name>` manage the store.
`set` reads the value from **stdin** (argv is same-user-readable on macOS and
world-readable on Linux). Known names only: `anthropic-api-key`,
`github-token`.

The server reads the store through a `Secrets` handle that checks the file's
mtime on access, so `tasks secrets set` **rotates a running server with no
restart** — the next GitHub poll, the next brokered request, uses the new
value. Raw values cross module boundaries only as `SecretString` (zeroized on
drop, `Debug` prints `***`, no `Display`), so a stray `{:?}` of a config can
never print one again.

Resolution order per key: sealed store (live) → process env at boot
(`ANTHROPIC_API_KEY` / `GITHUB_TOKEN`, kept as the dev/test path, warned as
deprecated for production) → for the Anthropic key only, the host's
`~/.claude/anthropic_key.sh` apiKeyHelper, as before. If a sealed store
*exists* but cannot be opened (missing unseal key), the server **refuses to
boot** — a server that silently came up without its keys would be the
".env silently reverted" failure with better branding.

### 2. Short-lived, scoped leases (`leases` table)

What a VM receives at dispatch is not a key but a **lease**: a random 256-bit
bearer token, stored **hashed** (SHA-256 — a copied database yields no live
credential), bound to its run (session id / build id), bound to its repo,
carrying explicit scopes, and expiring at the run's budget plus slack. Leases
are rows, not process state, because the process that mints one need not be
the process serving it — a reattach after a server restart extends the same
lease by subject; conclusion (success, failure, cancel) revokes best-effort,
and expiry is the backstop nothing can forget.

Scopes are where this gets stricter than the old world, not just equal to it:

- a Scout's lease is `anthropic` + `git-read`, bound to its project's repo;
- a Builder's lease is the same — **builders cannot push**; branch egress is
  a bundle and the push is the server's;
- the server's own land step mints itself a ~10-minute `git-read` +
  `git-write` lease per landing, so even host-side `git` argv never carries
  the PAT.

The classic PAT is all-or-nothing upstream; the broker is what makes a scoped
credential out of it.

### 3. The broker (`crates/tasks/src/broker.rs`)

A second HTTP listener in the server process (`TASKS_BROKER_PORT`, default
4801) — deliberately not the API listener, which stays loopback-only; this one
is reachable from the VM subnet (`bridge100`, host = `192.168.64.1` from
inside a VM, configurable via `TASKS_BROKER_ADVERTISE`). Every route demands a
valid lease. Two upstreams:

- **Git smart-HTTP passthrough**: VMs clone from
  `http://x-access-token:<lease>@192.168.64.1:4801/git/<owner>/<repo>.git`.
  The broker checks lease, scope (`git-upload-pack` ⇒ `git-read`,
  `git-receive-pack` ⇒ `git-write`) and repo binding, then streams the
  exchange to `github.com` with the real token injected as an `Authorization`
  header host-side. The token never crosses the vmnet in either direction.
  Lease-in-userinfo is what git echoes into errors, and both redaction layers
  (#970) already scrub URL userinfo — and the lease is dead minutes after the
  run anyway.
- **Anthropic passthrough**: VMs get `ANTHROPIC_API_KEY=<lease>` and
  `ANTHROPIC_BASE_URL=http://192.168.64.1:4801/anthropic`. Claude Code sends
  the lease as `x-api-key`; the broker swaps it for the real key and streams
  (SSE included) to `api.anthropic.com`. The agent authenticates every
  request of its run without ever holding the key — `container run`'s argv,
  the VM's environment, its disk, its transcript can leak nothing that
  outlives the run.

The name `ANTHROPIC_API_KEY` is kept for the lease on purpose: Claude Code
already reads it, and #970's name-based redaction already masks it in every
formatted environment.

## What this deliberately does not do

- **No new daemon.** The broker lives in the server process. The cost: a
  server restart severs in-VM agent API connections mid-response. That is
  exactly the transport death the in-VM supervisors already resume from
  (`--resume`, #845), and `reload --when-idle` avoids it entirely.
- **No GitHub App.** Installation tokens (genuinely short-lived, repo-scoped) would
  be better upstream credentials; the broker's mint/scope/expiry layer is
  where they would slot in later without touching VMs or images.
- **No TLS on the vmnet hop.** The subnet is host-local; what crosses it is
  lease tokens, which expire and are scope- and repo-bound. The raw keys only
  ever leave the process over TLS to the real upstreams.
- **No image rebuild.** Everything rides per-VM env and URLs the server
  already injects. Existing images work unchanged.

## Deployment and rotation runbook (the actual #971 remediation)

Written to be run top to bottom by a human at the machine. Steps 1–4 are
reversible and change no credential; step 5 is the irreversible half, and it
is deliberately last, so the mechanism is proven before the keys it protects
are replaced.

Two facts decide the ordering. The sealed store **outranks** the environment
fallback (`Secrets::get` reads the cache first), so sealing a key takes effect
immediately and *before* anything is deleted from `.env` — which means the old
value stays in place as a fallback the whole way through, and every step below
can be abandoned without losing the ability to serve. And a sealed store that
exists but cannot be opened **refuses to boot**, so the failure mode of a
locked login Keychain is a server that will not start, not one that quietly
falls back.

### 1. Deploy the code (no credential moves yet)

    make drain          # quiesce; new scouts and the build lane hold
    make restart
    make resume

`make restart` alone is enough if nothing is in flight. Two things to check
before you start:

- **Port 4801 must be free** — `lsof -nP -iTCP:4801 -sTCP:LISTEN`. The broker
  binds it at boot and a clash is a startup error by design. Override with
  `TASKS_BROKER_PORT` if something else owns it.
- **No image rebuild is needed.** The lease rides in `ANTHROPIC_API_KEY` and a
  clone URL, both of which the server already injects. `make images` is only
  on this list for the *unrelated* supervisor half of #970, at step 6.

Confirm it came up: `tasks status`, and `credential broker listening` in
`serve.log` with the advertised address beside it.

### 2. Create the sealed store

    tasks secrets init

Writes `<data dir>/secrets/sealed.json` (0600) and generates the unseal key
into the login Keychain (service `tasks-v2-secrets`). Neither artifact alone
decrypts anything, which is the whole property — so **back them up
separately, or not at all**. On a host where the login Keychain is locked
non-interactively (a headless or launchd-started server), use
`tasks secrets init --key-file PATH` instead and set `TASKS_SECRETS_KEY_FILE`;
`tasks secrets status` will name the override rather than the Keychain, which
is how you confirm it took.

### 3. Seal the keys you already have

Deliberately the ones currently in `.env` — this step proves the mechanism
end to end while the values are still ones you can afford to lose:

    printf %s "$ANTHROPIC_API_KEY" | tasks secrets set anthropic-api-key
    printf %s "$GITHUB_TOKEN"      | tasks secrets set github-token

Values come from **stdin, never argv** (argv is readable in `ps`). Pasting
interactively and pressing ctrl-D works too. A running server picks the change
up on its next read — rotation needs no restart — so verify with:

    tasks secrets status     # names and timestamps, never values

### 4. Prove the broker path before trusting it

Nothing here is destructive; all of it is reversible by `tasks secrets rm`.

1. **The keys are sealed, not stored.** `grep -r "$GITHUB_TOKEN"
   ~/.local/state/tasks-v2/secrets/` must find nothing.
2. **One scout, one real repo.** Queue a task and watch `serve.log` for
   `minted an agent lease` naming the session and repo. The run proves two
   distinct things at once: the clone succeeded (git went through
   `/git/owner/repo`) and the agent authenticated (Claude Code reached
   `/anthropic`). `RUST_LOG=tasks::broker=debug` turns on the per-request
   `git passthrough` / `anthropic passthrough` lines if you want to watch it
   happen.
3. **The VM holds no key.** In the run's transcript, `ANTHROPIC_API_KEY` is a
   `tl-` token, not an `sk-ant-` one, and it stops working minutes after the
   run ends.
4. **One build, through the landing push.** This is the only step that
   exercises the `land` lease — the agent lease deliberately cannot push, so a
   green scout says nothing about whether landing works.

If the Anthropic key is missing or unsealed, the broker answers `502` naming
the fix rather than failing obscurely; if GitHub's is, clones fall back to
anonymous and private repos `401` upstream.

### 5. Rotate (the irreversible half)

Only once step 4 is green:

1. Issue new credentials at both providers.
2. `tasks secrets set anthropic-api-key` and `tasks secrets set github-token`
   with the new values. The running server picks them up on its next read.
3. Confirm a fresh scout still works.
4. **Revoke the old credentials at the providers.** Until this happens the
   exposure #971 was filed about is unchanged — the keys are already in logs,
   consoles and archives, and nothing here can reach those. Sealing stops new
   writes; only revocation closes what is already out.
5. Delete both keys from every `.env`, shell profile and shell history.
   `<data dir>/.env` is easy to miss and is read by *every* `tasks`
   invocation. What confirms it is the `loaded .env` line each `tasks`
   command logs at startup, which **names the variables that file defined** —
   neither key should appear in it. The "raw `GITHUB_TOKEN` in the
   environment" warning does *not* answer this question: it is suppressed
   once the name is sealed, so it goes quiet at step 3 whether or not the
   `.env` entry is still there.

### 6. The rest of #971, unrelated to this change

Restart vm-pool (ahead of the server, per the pool's own upgrade rule) and run
`make images` for the supervisor halves of #970. Both are host acts, so they
go inside a `make drain` / `make resume` pair.

### Rollback

At any point before step 5: `tasks secrets rm <name>` (or delete
`<data dir>/secrets/`) and the environment fallbacks serve again on the next
read. After step 5 the old keys are revoked and there is nothing to roll back
to — which is why step 4 comes first.

## Env vars added

| var | default | |
| --- | --- | --- |
| `TASKS_BROKER_PORT` | 4801 | broker listener port |
| `TASKS_BROKER_BIND` | `0.0.0.0` | broker bind address (must be reachable from the VM subnet) |
| `TASKS_BROKER_ADVERTISE` | `192.168.64.1` | the broker's address as VMs see it (apple/container's bridge gateway) |
| `TASKS_BROKER_ANTHROPIC_UPSTREAM` | `https://api.anthropic.com` | override for tests |
| `TASKS_SECRETS_KEY_FILE` | — | unseal-key file; overrides the Keychain |

Git upstream reuses `GITHUB_CLONE_URL_BASE`.
