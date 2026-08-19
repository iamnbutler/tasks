# Release roadmap: the order of the road

*2026-08-18, written against `main` at 6248d18. #999 is the register — what
release requires and why, item by item. This document is the sequencing: what
goes first, what unblocks what, and who does which half. When the two
disagree, #999 is right about the *what* and this is right about the *when*.*

The near goal is not "released"; it is **dogfooding the release path** — the
pipeline building its own road-to-release queue while the human-only acts
(accounts, keys, decisions) happen in parallel. Two of tonight's failures
(#1006, #1008) are what stands between here and leaving `play` on.

## Phase 0 — make the pipeline honest again (before anything else)

Tonight the pipeline destroyed two tasks over a one-minute broker outage and
recorded a dead Builder as `succeeded`, opening PR #1007 with unimplemented
specs behind a generated body. Both are p0 because the entire premise —
unattended work — rests on them:

1. **#1008** — a Builder whose agent exits nonzero must not be `succeeded`.
   No `SUMMARY.md`, no success. The PR body must never be a generated claim
   about work nobody did.
2. **#1006** — broker health holds dispatch, the same shape as
   `GitHubHealth` (#939's pattern). A hold, **not** a strike waiver:
   CLAUDE.md already names why waiving pre-agent setup failures is the fix
   that looks equivalent and is not.
3. **Recovery**: the outage's victims sit `rejected` at the attempt cap —
   tasks for #982 and #996. The fix should refund strikes charged during a
   provable outage window, or they get a one-off reset. Requeue both.

Already moving: the supervisor-leak fix (separate session, in flight) and
#1010 (39 GB `ORCHESTRATOR_TARGET_DIR`), which is queued in-pipeline.

**Until phase 0 lands, the pipeline stays paused** and tonight's work happens
in direct sessions — which also means no collision between the two.

## Phase 1 — five workstreams, sequenced by lead time

The registrations go first because their lag is external; code fills the gap.

| # | workstream | first act | owner of first act |
| --- | --- | --- | --- |
| 1 | Signing (#988 — decided: notarized download) | Apple Developer enrollment | Nate, tonight — activation takes days |
| 2 | Device flow (#1002 — decided: OAuth App) | Register the OAuth App, get the `client_id` | Nate, tonight — five minutes |
| 3 | Modals (#1013) | Build the layer; port palette + Stop confirm; then #992 → #993 → #1005 | Opus tonight |
| 4 | CI (#1015, split from #996) | Landing-rule move + ubuntu workflow, informational | Opus tonight |
| 5 | Release flow (#997 + #1014) | `make images-push` / `tasks images pull`; `make release` scaffolding unsigned | Opus, then blocked on 1 |

Decisions recorded (in the issues, dated 2026-08-18):

- **#1002**: OAuth App, not GitHub App — token doesn't expire, sealed store
  unchanged. Revisit GitHub App only if orgs ever care.
- **#988**: Option B, signed + notarized. #991/#995/#997 branch on this and
  may now assume a notarized artifact.
- **#997**: tags are `v0.1.<commit count>`, matching the build stamp — one
  number everywhere. Releases are checkpoints of `main`, not a train.
- **#1015**: CI starts informational (no required checks, no branch
  protection); the `land_builds` carve-out (c), `Landing::Clear::describe()`,
  its pinning test and the CLAUDE.md bullet move in the same change as the
  workflow file.

## Phase 2 — the pipeline eats its own queue

Turn `play` back on once phase 0 lands (checklist below). Already queued:
#983 #984 #985 #987 #988 #989 #990 #992 #993 #994 #995 #1010 — the
road-to-release batch. Already `ready_to_build`: the #1007 batch (#930 #950
#967 #973), properly unwound, plus #1003. Post-#1013, the app trio
(#992 #993 #1005) flows.

Stays backlog, queue selectively: the remaining pipeline bugs
(#949 #962 #965 #968 #976 #981 #982), docs debt (#932 #969), and #936
(needs a Mac and eyes, not an agent).

## Tonight's blast list, in order

1. #1008 — Builder honesty (worktree A)
2. #1006 — broker hold + strike refund (worktree B; independent of A)
3. #1013 — modal layer, then as far into #993/#1005 as the evening goes
4. #1015 — CI + the landing-rule move
5. #985 — Origin/Host check (small, security, while nobody's using it)
6. #983 — LICENSE (trivial; close it by hand rather than spending a scout)

Meanwhile, human lane: Apple enrollment, OAuth App registration, and the
#971 runbook (rotate, then images, then pool, then server) — the last gate
before anyone else is invited in.

## Restart checklist (after phase 0)

```sh
tasks service start        # boots paused: API up, no dispatch
tasks images pull || make images   # images are behind (0.1.583 < server)
# requeue the outage victims (#982, #996), verify strikes refunded
curl -s -X POST localhost:4800/mode -d '{"mode":"play","note":"phase 0 landed; resuming the road-to-release queue"}'
```

## First release, defined

A `v0.1.<n>` tag whose GitHub release carries a notarized app + `tasks`
binary, images published under the same stamp (#1014), a README a stranger
can follow (#994), a LICENSE (#983), the disclaimer (#984), and the Origin
fix (#985). Everything else on #999 improves it; those seven gate it.
