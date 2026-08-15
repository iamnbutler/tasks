//! tasks: human-in-the-loop agent orchestration server.
//!
//! Domain and wire types live in the `tasks-api` crate (re-exported here as
//! `models` and `events`) so native clients can share them without depending
//! on the server stack.

pub mod brief;
pub mod briefing;
pub mod builder;
pub mod github;
pub mod orchestrator;
pub mod pidfile;
pub mod reload;
pub mod run;
pub mod scout;
pub mod server;
pub mod store;
mod teardown;
pub mod transcript;

pub use tasks_api::{events, models};
pub use tasks_protocol as protocol;
