//! Center surfaces. Each file extends `Workspace` with one or more render
//! methods; they read `AppState` and talk back through workspace listeners —
//! no state of their own.
//!
//! `tasks` is the All Tasks catalog; `detail` is the Overview and Brief
//! tabs; `changes` is the Changes tab. The v1 sections (`home`, `activity`,
//! `queue`) were deleted by v3's milestone 5 — the queue's reorder math
//! lives on in `crate::rail`, and Activity's feed returns as an
//! orchestrator-side surface later (docs/plans/2026-08-17-v3-ui.md §8).

mod changes;
mod detail;
mod tasks;
