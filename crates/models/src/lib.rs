//! Shared domain model types for the Tasks platform (spec Section 5).
//!
//! These types are used by both the server and the store crates.

pub mod automation;
pub mod parked_question;
pub mod task;
pub mod session;
pub mod merge_queue;
pub mod mode;
pub mod project;

pub use mode::Mode;
