//! tasks: human-in-the-loop agent orchestration server.
//!
//! This library is split into modules: `models` (domain types), `store` (SQLite
//! persistence). More modules land as we implement each step of the Diamond 1 plan.

pub mod briefing;
pub mod builder;
pub mod events;
pub mod github;
pub mod models;
pub mod orchestrator;
pub mod run;
pub mod scout;
pub mod server;
pub mod store;

pub use tasks_protocol as protocol;
