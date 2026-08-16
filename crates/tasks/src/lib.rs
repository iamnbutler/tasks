//! tasks: human-in-the-loop agent orchestration server.
//!
//! Domain and wire types live in the `tasks-api` crate (re-exported here as
//! `models` and `events`) so native clients can share them without depending
//! on the server stack.

pub mod brief;
pub mod briefing;
pub mod builder;
pub mod cancel;
pub mod env_file;
pub mod github;
/// The migration set plus the naming rule that keeps parallel branches from
/// colliding on it — private, like `teardown`: the store is the only caller.
mod migrations;
pub mod orchestrator;
pub mod pidfile;
pub mod reattach;
pub mod redact;
pub mod reload;
pub mod run;
pub mod scout;
pub mod server;
pub mod store;
mod teardown;
pub mod transcript;
pub mod version;

pub use tasks_api::{events, models};
pub use tasks_protocol as protocol;
