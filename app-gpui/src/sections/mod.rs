//! Per-section center surfaces. Each file extends `Workspace` with the
//! render method for one sidebar section; they read `AppState` and talk
//! back through workspace listeners — no state of their own.

mod activity;
mod detail;
mod queue;
mod tasks;
