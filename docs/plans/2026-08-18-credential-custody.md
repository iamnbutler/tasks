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

## Rotation runbook (the actual #971 remediation, after this deploys)

1. `make restart` onto this change. `tasks secrets init`, then rotate both
   keys at their providers, then `tasks secrets set anthropic-api-key` and
   `tasks secrets set github-token` (paste, ctrl-D).
2. Delete both keys from every `.env` and shell profile.
3. Restart vm-pool (per #971, ahead of the server) and `make images` for the
   supervisor halves of #970 — unrelated to this change but part of the same
   runbook.

## Env vars added

| var | default | |
| --- | --- | --- |
| `TASKS_BROKER_PORT` | 4801 | broker listener port |
| `TASKS_BROKER_BIND` | `0.0.0.0` | broker bind address (must be reachable from the VM subnet) |
| `TASKS_BROKER_ADVERTISE` | `192.168.64.1` | the broker's address as VMs see it (apple/container's bridge gateway) |
| `TASKS_BROKER_ANTHROPIC_UPSTREAM` | `https://api.anthropic.com` | override for tests |
| `TASKS_SECRETS_KEY_FILE` | — | unseal-key file; overrides the Keychain |

Git upstream reuses `GITHUB_CLONE_URL_BASE`.
