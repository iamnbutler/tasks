# PR Merge Strategy

Generated: 2026-03-29

## Overview

29 open PRs organized into 5 tiers by priority and risk. All PRs currently fail CI due to stale `.lock()` calls fixed in main - each needs a rebase before merge.

---

## Tier 1: Critical Bug Fixes & Infrastructure
**Merge first. Small, low-risk, fix real issues.**

| PR | Size | Title | Notes |
|----|------|-------|-------|
| #620 | +14/-12 | Fix stale conflict entries without timestamps | Bug fix, tiny |
| #617 | +33/-27 | Isolate mode into its own Mutex for atomic transitions | Race condition fix |
| #616 | +158/-1 | Validate blocked_by to prevent circular dependencies | Prevents deadlocks |
| #614 | +31/-1 | Add container runtime health check at startup | Fails fast on missing CLI |
| #613 | +39/-1 | Add exponential backoff on GitHub poll failures | Prevents API hammering |
| #652 | +33/-5 | Add HTTP timeouts to prevent poll failures | Prevents server stalls |
| #628 | +143/-2 | Deduplicate poll results by node_id | Data integrity fix |

**Order:** 620 → 617 → 616 → 614 → 613 → 652 → 628

---

## Tier 2: Core Model Improvements
**Needed as foundation for other features.**

| PR | Size | Title | Notes |
|----|------|-------|-------|
| #618 | +63/-5 | Replace untyped Project.config with typed ProjectConfig | Cleanup, backward compatible |
| #619 | +248/-4 | Add ClosureReason to distinguish agent success | Needed for orchestrator |
| #625 | +82/-18 | Add compiled_at timestamp for stale workflows | Needed for automations |
| #627 | +186/-0 | Add CI status, reviews, reactions to PR queries | Needed for orchestrator |

**Order:** 618 → 619 → 625 → 627

---

## Tier 3: Web UI Improvements
**Low risk, user-facing value. Can merge in any order.**

| PR | Size | Title | Notes |
|----|------|-------|-------|
| #651 | +21/-9 | Communicate Pause mode behavior changes | Tiny, tooltips |
| #647 | +28/-0 | Display orchestrator rejection feedback | Tiny, additive |
| #643 | +77/-1 | Add version display and data-clear prompt | Small |
| #623 | +153/-4 | Add Request Changes button to merge queue | Small |
| #642 | +140/-14 | Show task state transitions and surface errors | Helpful UX |
| #649 | +129/-2 | Add 'Rebuild from GitHub' button | Useful |
| #644 | +109/-5 | Show rejected-PR cooldown status | Nice to have |
| #654 | +133/-6 | Desktop app retry with backoff | Desktop only |

**Order:** 651 → 647 → 643 → 623 → 642 → 649 → 644 → 654

---

## Tier 4: Orchestrator Features
**Large features. Merge in dependency order.**

| PR | Size | Title | Notes |
|----|------|-------|-------|
| #630 | +367/-3 | dispatch_agent capability | **FOUNDATION** - merge first |
| #633 | +318/-28 | Proactive stream-of-consciousness narration | Builds on #630 |
| #634 | +505/-10 | Triage and decomposition (spec §4.2) | Standalone |
| #632 | +676/-52 | Spawn investigation agents during PR eval | Builds on #630 |
| #635 | +800/-44 | Autonomous behavior when human disconnected | Builds on #630 |
| #636 | +752/-19 | Diagnose failures and suggest recovery | Large, review carefully |
| #638 | +509/-16 | Manage task priority and dispatch ordering | **CONFLICTS with #659** |

**Order:** 630 → 633 → 634 → 632 → 635 → 636 → 638

**Important:**
- Merge #659 (centralized work queue) before #638 - both modify `run_loop.rs` dispatch logic
- #638 will need significant rework after #659 merges

---

## Tier 5: Complex/Defer
**Needs careful review or can wait.**

| PR | Size | Title | Recommendation |
|----|------|-------|----------------|
| #624 | +266/-90 | Typed event payloads for compile-time safety | Review carefully - large type refactor |
| #641 | +320/-16 | Wire up GitHub write operations to web UI | Review carefully - write operations |
| #639 | +562/-11 | Post-merge reflection system (Phase 1) | **DEFER** - new feature, not urgent |
| #653 | +518/-71 | Context window management with compaction | **DEFER** - complex, needs thorough review |

---

## Recommended Discard: None

All PRs appear legitimate. However:
- **#653** is complex enough to warrant deferring until we have bandwidth for thorough review
- **#639** introduces a new feature (reflections) that isn't blocking anything

---

## Conflict Notes

1. **#659 vs #638**: Both modify dispatch logic in `run_loop.rs`. Merge #659 first, then update #638 to use the new work queue API for priority management.

2. **Rebase all**: Every PR needs rebase on main to pick up the `.lock()` removal from #655.

---

## Quick Merge Batch (Today)

For immediate progress, rebase and merge these 10 small PRs:

```
# Tier 1 bug fixes
620, 617, 614, 613, 652

# Tier 2 models
618

# Tier 3 tiny UI
651, 647, 643
```

Each is small, low-risk, and provides immediate value.
