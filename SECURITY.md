# Security

## Reporting

Report privately through GitHub's **Report a vulnerability** button on the
[Security tab](https://github.com/iamnbutler/tasks/security/advisories/new).
Do not open an issue: this repository's own pipeline reads its issue tracker,
and an issue describing a way to reach a credential is an issue an agent may
read, summarise into a spec and quote into a pull request body.

Include what you would want if you were fixing it — the version
(`tasks status`, or `curl localhost:4800/version`), the call site,
and what an attacker gets. If it involves a credential, name the **variable,
the field or the call site**, never the value: a token pasted into a report is
a token in GitHub's storage, in an email notification and in the reporter's
sent mail, and rotating it becomes the first thing anyone does before the bug
is even understood. That is not hypothetical here — #923 and #971 were about a
credential reaching a log, and both were diagnosed and fixed without a
fragment of one appearing anywhere.

## What is in scope

The interesting boundaries, in the order they are load-bearing:

- **The credential broker** (`crates/tasks/src/broker.rs`, port 4801). It is
  the one listener reachable from the VM subnet, and a VM holds a lease rather
  than a key. Anything that gets a lease to spend a scope it was not granted —
  a Scout lease reaching `git push`, a lease outliving its run, one run's lease
  serving another's repository — is the highest-value bug in the tree.
- **Sealed credential custody** (`<data dir>/secrets/`, the unseal key in the
  OS credential store). Neither artifact alone should decrypt anything.
- **The loopback guard** (`crates/tasks/src/loopback.rs`). The local API has no
  authentication, so a request that does not identify itself is read as the
  human and the human is never gated. The guard exists to keep a *browser* off
  that API — DNS rebinding and CORS-simple `POST`s. A way past it from a page
  is in scope. A way past it from another process on the same machine is not:
  see below.
- **The charter and the decisions ledger.** A GitHub write that reaches the
  API without a `decisions` row explaining it, or one that passes a capability
  set to `off` or `shadow`, is in scope.
- **The agent/VM boundary.** Anything an agent inside a VM can reach that its
  lease and its scopes do not permit.

## What is not

These are known, documented and deliberate. Reporting one is welcome as an
*issue*, not as a vulnerability:

- **Any local process can drive the pipeline.** The API is loopback-only and
  unauthenticated by design; a caller with no `X-Tasks-Actor` is the human.
  Who can run code on the machine is the boundary, and it is the same boundary
  as the shell that started the server.
- **Agents run with permission checks off inside their VM.** The VM is the
  containment, not the permission prompt. See `## Read this first` in the
  README.
- **The orchestrator is not in a VM.** What it may do is whatever
  `ORCHESTRATOR_CMD` allows, and pointing it at a checkout with
  `--dangerously-skip-permissions` is a supported way to run it.
- **A `GET` subresource load from a page.** Browsers send no `Origin` on
  `<img src>`/`<script src>`, so a cross-site load of a loopback `GET` passes
  both loopback rules. The residual is bounded to responses the attacker
  cannot read and effects that are server-side; the two non-nil routes are
  named in `CLAUDE.md`.

## There is no embargo process, because there is no release train

This matters to a reporter, so it is stated rather than left to be discovered.

There are no releases, no tags and no versioned artifacts. A fix lands on
`main` and that is the whole publication. There is nothing to coordinate a
disclosure date against, no backport, and no way to ship a patch to a running
install other than the operator pulling and restarting.

**A merged fix is therefore not a deployed fix**, and the gap is larger than
it is in most projects:

- a **server** fix reaches nothing until `make restart` on the host;
- a fix inside a **VM image** — the supervisors, the agent commands, anything
  under `images/` — reaches nothing until someone runs `make images` on a Mac
  with the container CLI and the cross toolchain, and then restarts;
- a **vm-pool** fix needs the pool restarted, which is a separate daemon and a
  separate act (`make drain` → restart → `make resume`).

So the honest timeline is: report privately, we fix it on `main`, and the
advisory is published once the fix is in — with the restart steps in it,
because for the operator reading it those *are* the remediation. If you need a
date to coordinate around, say so in the report and we will agree one; the
default is "as soon as it is fixed", since a merged fix is public in the commit
log the moment it lands.

Nobody is on call. This is software one person wrote to run on his own
machine.
