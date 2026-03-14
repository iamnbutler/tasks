//! Tasks server — the platform.
//!
//! Spec Section 3.1: The server is the long-running process that hosts
//! the event log, task state, merge queue, and scheduler.

pub mod model;
pub mod mode;
pub mod merge_queue;
pub mod presence;
pub mod prompt;
pub mod workflow;
mod server;

pub use mode::Mode;
pub use server::{Server, ServerError, ServerState};
