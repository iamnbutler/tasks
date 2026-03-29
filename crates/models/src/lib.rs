//! Shared domain model types for the Tasks platform (spec Section 5).
//!
//! These types are used by both the server and the store crates.

pub mod automation;
pub mod task;
pub mod session;
pub mod merge_queue;
pub mod mode;
pub mod project;
pub mod work_queue;

pub use mode::Mode;
pub use work_queue::{WorkType, WorkItem, ClaimResult, ReclaimedWork};
