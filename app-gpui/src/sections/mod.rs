//! Center surfaces. Each file extends `Workspace` with one render method;
//! they read `AppState` and talk back through workspace listeners — no state
//! of their own.
//!
//! Since the v3 frame swap (docs/plans/2026-08-17-v3-ui.md) only `tasks` (the
//! All Tasks catalog) and `detail` (the Overview tab's content, pending its
//! milestone-3 split into tabs) are reachable. `home`, `activity` and `queue`
//! are kept compiling until milestone 5 deletes them — `queue` in particular
//! still holds the reorder-payload math the left rail's drag ranking (M2)
//! ports.

#[allow(dead_code)]
mod activity;
mod detail;
#[allow(dead_code)]
mod home;
#[allow(dead_code)]
mod queue;
mod tasks;
