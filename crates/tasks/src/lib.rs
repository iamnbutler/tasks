//! tasks: human-in-the-loop agent orchestration server.
//!
//! This library is split into modules: `models` (domain types), `store` (SQLite
//! persistence). More modules land as we implement each step of the Diamond 1 plan.

pub mod models;
pub mod store;
