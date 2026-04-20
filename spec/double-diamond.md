# Double Diamond Architecture

Status: Draft — RFC #744

This document specifies the Double Diamond execution model: parallel exploration ("Scouts")
feeding a serial implementation pipeline ("Builder"). It supersedes the current "N parallel
implementers" model documented in spec.md §5 and §10.

Companion to the [RFC](https://github.com/iamnbutler/tasks/issues/744), the main [spec](./spec.md),
and [session-runtime.md](./session-runtime.md). Nothing in this document changes the session
runtime itself — Scouts and Builders are ordinary sessions with different prompts and different
completion semantics.

## 1. Goals & Non-Goals

**Goals.**

- Eliminate merge conflicts between in-flight implementations by making the implementation phase
  strictly serial.
- Capture exploration knowledge (pitfalls, non-obvious dependencies) as persistent artifacts that
  survive rejection and re-attempts.
- Give the Builder a stable, known `main` to integrate against, with context about recently merged
  and upcoming work.
- Keep the existing session runtime, container lifecycle, and merge queue mechanics unchanged.

**Non-goals.**

- Replacing the orchestrator, the work queue, or the merge queue as components. The Double
  Diamond is a reshape of what flows through them.
- Solving long-running design questions (multi-week feature specs). This targets implementable
  issues — the same class of work handled today.
- Guaranteeing zero Builder failures. Scouts reduce surprises but cannot eliminate them; the
  feedback loops in §7 cover the residual cases.

## 2. Terminology

| Term | Definition |
|---|---|
| **Scout** | Session in Diamond 1. Produces a **Spec** by implementing the feature on a throwaway branch. |
| **Spec** | Markdown artifact distilled from a Scout's implementation experience. The only output Diamond 1 persists. |
| **Spec Queue** | Ordered list of approved Specs awaiting a Builder. |
| **Builder** | Session in Diamond 2. Implements a Spec serially against a stable `main`. Produces a PR. |
| **Change Queue** | Existing merge queue (`crates/server/src/merge_queue.rs`), unchanged in role. |

## 3. Data Model

Three new first-class types, plus extensions to `Task`.

### 3.1 Task kinds

`Task` gains a `kind` discriminator. Today every task is implicitly "implement this issue." We
split that into three kinds that share the existing `Task` row but differ in state machine and
dispatch:

```rust
pub enum TaskKind {
    /// Legacy single-phase task. Retained for migration & simple/fast-path work (§8).
    Implement,
    /// Diamond 1 exploration. Produces a Spec. Branch is throwaway.
    Scout { issue_task_id: String, attempt: u32 },
    /// Diamond 2 implementation. Consumes a Spec. Produces a PR.
    Builder { spec_id: String },
}
```

Rationale for a single `Task` row with a `kind` field (rather than three tables): the existing
dispatch, session, failure, retry, rejection-feedback, and accounting machinery already operates
on `Task`. Forking into three tables would double that surface area for no benefit.

### 3.2 Spec

```rust
pub struct Spec {
    pub id: String,
    pub issue_task_id: String,      // The "parent" task representing the user-facing issue.
    pub scout_task_id: String,      // Which Scout attempt produced this Spec.
    pub content: String,            // Markdown, structured per §4.3.
    pub complexity: Complexity,
    pub dependencies: Vec<String>,  // issue_task_ids that must merge first.
    pub files_touched: Vec<String>, // Hint for Builder & queue prioritization (§5.2).
    pub status: SpecStatus,
    pub revision: u32,              // Bumped on re-exploration.
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub enum SpecStatus {
    PendingReview,    // Scout just posted; orchestrator hasn't looked yet.
    Approved,         // In Spec Queue, awaiting Builder.
    NeedsRevision,    // Orchestrator sent feedback; Scout task re-dispatched.
    Blocked,          // Dependencies not yet merged.
    Consumed,         // A Builder has claimed this Spec.
    Superseded,       // Replaced by a later revision.
    Rejected,         // Issue withdrawn / re-exploration requested from scratch.
}

pub enum Complexity { Simple, Medium, Complex }
```

### 3.3 Issue-level task

The user-facing issue becomes an **umbrella task** that holds the link between its Scout
attempts and the resulting Spec/Builder. Its state reflects the phase:

```
   new issue
      │
      ▼
  ExploringScout  ─────┐  (Scout tasks dispatched as children)
      │               │
      ▼               ▼
   SpecReview    SpecBlocked
      │               │
      ▼               │
   QueuedForBuilder ◄─┘
      │
      ▼
   BuildingImpl   (a Builder task is dispatched as child)
      │
      ▼
   AwaitingMerge  (existing state; Builder PR in Change Queue)
      │
      ▼
   Completed / Failed / Cancelled
```

Reuses as many existing `TaskState` variants as possible. New variants needed:
`ExploringScout`, `SpecReview`, `SpecBlocked`, `QueuedForBuilder`, `BuildingImpl`. All are
non-terminal and route back to existing terminal states.

### 3.4 Storage

Two new SQLite tables (migration in `crates/store/src/schema.rs`):

```sql
CREATE TABLE specs (
    id             TEXT PRIMARY KEY,
    issue_task_id  TEXT NOT NULL,
    scout_task_id  TEXT NOT NULL,
    content        TEXT NOT NULL,
    complexity     TEXT NOT NULL,
    dependencies   TEXT NOT NULL,          -- JSON array
    files_touched  TEXT NOT NULL,          -- JSON array
    status         TEXT NOT NULL,
    revision       INTEGER NOT NULL DEFAULT 1,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    FOREIGN KEY (issue_task_id) REFERENCES tasks(id),
    FOREIGN KEY (scout_task_id) REFERENCES tasks(id)
);
CREATE INDEX idx_specs_issue    ON specs(issue_task_id);
CREATE INDEX idx_specs_status   ON specs(status);

-- tasks table gains:
--   kind TEXT NOT NULL DEFAULT 'implement'
--   kind_payload TEXT             -- JSON-encoded Scout/Builder payload
```

## 4. Diamond 1: Exploration

### 4.1 Dispatch

When an issue enters `ExploringScout`, the orchestrator enqueues **one** Scout task as a child.
The RFC describes "N parallel Scouts" per issue, but for v1 we dispatch one per issue and let the
orchestrator fan-out only when explicitly requested (e.g., known-controversial issue, label
`scout:multi`). Fan-out has a real cost — Scouts run full implementations — and the non-multi case
should remain the default until we have data.

### 4.2 Scout workflow (prompt-level)

1. Claim issue, create throwaway branch `scout/<issue_task_id>-<attempt>`.
2. Implement the feature end-to-end. Tests must pass. Lint must pass.
3. If implementation is impossible or reveals a blocker, **stop and emit a Spec that documents
   why** rather than retrying blindly.
4. Emit the structured Spec markdown (§4.3) as the session's final message.
5. Session exits; the host parses the Spec, persists it, transitions the issue task to
   `SpecReview`, and deletes the throwaway branch.

### 4.3 Spec template

See the RFC for the normative structure. The prompt will require every section; missing or empty
sections are an orchestrator rejection reason.

### 4.4 Failure handling

- **Scout crashes** → same retry policy as today's failed tasks (up to `retry_count` limit).
- **Scout completes but emits invalid / empty Spec** → Orchestrator triggers re-exploration with
  feedback, revision increments.
- **Scout exceeds `CONTAINER_TIMEOUT`** → killed as today; treated as retryable if transient.

## 5. Spec Queue

### 5.1 Review

Orchestrator reviews a `PendingReview` Spec at its next `think()` tick. It checks the Spec
structure is complete, pitfalls are named, dependencies are identifiable, and complexity is a
plausible match for the files touched. This is cheap — it's reading markdown, not code.

Outputs: `Approved`, `NeedsRevision(feedback)`, or `Rejected(reason)`.

### 5.2 Prioritization

In priority order:

1. **Dependencies met** — every Spec in `dependencies` has an associated merged PR.
2. **Explicit priority** — `priority` field on the issue task, same semantics as today.
3. **Low conflict risk** — Specs touching files unrelated to in-flight Builder work jump ahead.
   (Uses `files_touched` from §3.2.)
4. **Complexity** — `Simple` before `Medium` before `Complex`, configurable per project.
5. **Age** — FIFO within priority bands.

Rule #3 is the only new one. It exists because serial Builder execution means queue position
translates directly to latency for the user — prefer Specs that won't block on one another.

### 5.3 Staleness

A Spec is **stale** if its `files_touched` overlap the diff of any PR merged after the Spec was
created. Stale Specs are demoted to `NeedsRevision` with a pointer to the merged PR(s). The
orchestrator may opt to re-Scout (new revision) or dispatch the Builder and let it reconcile.

## 6. Diamond 2: Implementation

### 6.1 Dispatch

Exactly **one Builder runs per project at a time**. Per-project, not global, so that independent
repos don't block each other.

The work queue gains a `WorkType::Builder = 1` tier (between MergeConflict and PrFeedback) and the
dispatcher refuses to claim a Builder slot while a Builder is active for the same project.

### 6.2 Builder context bundle

```rust
pub struct BuilderContext {
    pub spec: Spec,
    pub in_flight: Vec<InFlightWork>,    // Concurrent Scouts, for awareness only.
    pub recent_merges: Vec<MergedPr>,    // Last N=20 merges on main.
    pub upcoming_specs: Vec<SpecSummary>,// Next ~5 in the queue.
    pub main_sha: String,
}
```

This is assembled in `crates/server/src/prompt.rs` alongside `PromptParams`. The Builder prompt
instructs the agent to treat the Spec as binding, use `recent_merges` to avoid re-doing already
done work, and cite `upcoming_specs` only when an architectural choice would conflict.

### 6.3 Builder workflow

Structurally identical to today's implementer workflow. Builder creates
`impl/<issue_task_id>-<slug>`, implements, tests, opens a PR, and the PR enters the Change Queue
unchanged. The difference is that the Builder has the Spec and context bundle up front, and is
guaranteed no concurrent implementer is touching the tree.

## 7. Feedback Loops

| Scenario | Detected in | Routed to |
|---|---|---|
| Spec incomplete / wrong | Orchestrator review | Scout re-exploration (new revision) |
| Spec needs minor revision | Orchestrator review | Same Scout task, `NeedsRevision` |
| Builder blocks on Spec gap | Builder session | Cancel Builder → Scout re-exploration |
| CI fails on PR | Change Queue | Builder task, `ChangesRequested` |
| Trivial merge conflict | Change Queue | Auto-resolve + CI re-run (today's behavior) |
| Non-trivial merge conflict | Change Queue | Builder re-dispatch with merge context |
| Human requests changes | Change Queue | Builder task, `ChangesRequested` |
| Stale Spec (files moved) | Queue manager | `NeedsRevision` + optional re-Scout |

Routing decisions are made by the orchestrator, not the Builder. This keeps the agent from making
architectural decisions about its own pipeline.

## 8. Fast Path

For trivially small changes (single-file docs fix, obvious typo, dependency bump), the Double
Diamond is overkill. The existing `TaskKind::Implement` path is retained for:

- Tasks with label `fastpath`.
- Tasks the orchestrator classifies as trivial during `think()` (heuristic: description < 200
  chars AND labels ∩ {docs, typo, deps} non-empty).
- Any task where a human explicitly requests skipping the Scout phase.

Fast-path tasks do not enter the Spec Queue and do not block the Builder slot.

## 9. Human Interaction

Three injection points, reachable via existing API endpoints:

1. **Write a Spec manually.** `POST /api/specs` with a Spec body bypasses Diamond 1 entirely and
   enqueues a Builder directly. The Spec is still subject to orchestrator review (it can be
   rejected or flagged as `NeedsRevision` just like a Scout's).
2. **Approve/reject a Spec.** `POST /api/specs/:id/approve|reject`, mirroring merge queue
   approval. In Pause mode, all Specs require explicit approval.
3. **Override Spec Queue order.** `POST /api/specs/:id/priority` to boost or defer.

Rejection of a PR in the Change Queue uses the existing `rejection_feedback` path, but routing
(fix vs re-explore vs re-Scout) is determined by the orchestrator from the feedback text and PR
state.

## 10. Rollout Plan

### Phase 1 — Data model (no behavior change)

- Add `TaskKind`, `Spec`, `SpecStatus`, `Complexity` types in `crates/models`.
- Add `specs` table and `tasks.kind`, `tasks.kind_payload` columns via migration.
- Add `spec` and `specs` CRUD to `crates/store`.
- Existing tasks all default to `TaskKind::Implement`; no runtime behavior changes.
- **Ship independently of Phases 2–4.**

### Phase 2 — Scout path

- New Scout prompt template.
- Scout dispatch when an issue has label `scout` or project config opts in.
- Spec parsing on session completion.
- Orchestrator review at `think()` tick.
- No Builder integration yet — Specs accumulate and are readable via `GET /api/specs`.

### Phase 3 — Builder path + Spec Queue

- `WorkType::Builder`, dispatcher slot enforcement (§6.1).
- Builder prompt + context bundle.
- Spec Queue ordering (§5.2).
- End-to-end: issue → Scout → Spec → Builder → PR → merge.

### Phase 4 — Feedback loops + staleness + UI

- Implement §7 routing.
- Spec staleness detection.
- Web UI: Spec Queue view, Spec viewer, manual Spec submission form.
- Desktop UI: same.

### Migration

No data migration beyond the schema change. In-flight tasks at rollout time continue as
`TaskKind::Implement` and finish via the legacy path. New tasks created after Phase 2 ships can
opt into the Scout path.

## 11. Open Questions (Resolved)

Critical open questions from the RFC, with proposed resolutions:

- **#1 Spec storage.** SQLite (primary) + GitHub comment (mirror). DB is authoritative; comment
  is human-readable and survives platform reinstall.
- **#2 Spec versioning.** Keep history (`revision` field, §3.2). Cheap, and useful for the
  "stale Spec" re-exploration case.
- **#3 Scout branch cleanup.** Delete immediately on Spec submission. The Spec is the artifact;
  the branch is not. Recovery (if needed) is "re-Scout."
- **#4 Scout reuse for revisions.** Yes — same `scout_task_id`, `revision` bumps. Keeps history.
- **#5 Scout failure.** Partial Spec allowed (must document why). Routed to orchestrator for
  either human escalation or re-dispatch.
- **#6 Builder timeout.** Same as today (`CONTAINER_TIMEOUT`, default 2h).
- **#7 Builder stuck protocol.** Builder asks a Question (existing state). Orchestrator answers
  from Spec + context, escalates to human if answer requires re-exploration.
- **#8 Fast path.** §8 above.
- **#9 Spec Queue size.** Soft cap 50 approved Specs per project; Scouts pause dispatch above the
  cap to avoid wasted exploration.
- **#10 Staleness.** §5.3.
- **#11 Priority changes.** Allowed any time, via the existing priority field on the issue task.
- **#12 Orchestrator review depth.** Structural + sanity check, not a full architectural review.
  Keep it cheap; let Builder failures surface deep problems.
- **#13 Orchestrator Scout/Build directly.** No. The orchestrator orchestrates; it doesn't code.
  Tempting but collapses the review/implement separation that makes the Double Diamond work.
- **#14 Rejection routing.** Orchestrator decides, per §7.
- **#15 Previous-attempt context in re-exploration.** Full previous Spec + orchestrator feedback
  is included. The whole point is to not waste learnings.
- **#16 Human injection points.** §9.
- **#17 Skip exploration for detailed issues.** Only via label opt-in (`fastpath`), not
  heuristic. Detailed-looking issues are often the ones where exploration matters most.
- **#18 Metrics.**
  - Spec-to-merge rate (Specs that result in a merged PR / total approved Specs).
  - Re-exploration rate (Specs with revision > 1 / total Specs).
  - Time-in-phase (queue to Builder claim, Builder claim to PR, PR to merge).
  - Scout wasted-work rate (Scouts whose Specs were never consumed / total Scouts).

## 12. Risks

- **Scout cost.** Each Scout runs a full implementation. For a repo with high task throughput,
  that's a significant API spend for work we throw away. Mitigation: fast path (§8), opt-in per
  project, metrics on Scout wasted-work rate.
- **Serial Builder as bottleneck.** One Builder per project means Builder latency becomes
  user-visible. Mitigation: per-project serialization (not global), Spec prioritization (§5.2).
- **Spec rot.** A Spec that sits in queue while `main` advances can mislead the Builder.
  Mitigation: staleness detection (§5.3), freshness bias in prioritization.
- **Orchestrator review as new bottleneck.** If review is slow, Spec Queue stalls. Mitigation:
  keep review cheap (§11 #12), batch-review at `think()` ticks.
