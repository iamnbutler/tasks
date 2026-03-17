//! Tasks server — the platform.
//!
//! Spec Section 3.1: The server is the long-running process that hosts
//! the event log, task state, merge queue, and scheduler.

pub mod model;
pub mod mode;
pub mod merge_queue;
pub mod presence;
pub mod prompt;
pub mod recovery;
pub mod workflow;
pub mod dispatcher;
pub mod scheduler;
pub mod workspace;
mod server;

pub use mode::Mode;
pub use recovery::{RecoveryResult, DEFAULT_MAX_RETRIES};
pub use server::{Server, ServerError, ServerState};
pub use workspace::{
    WorkspaceCleanupManager, WorkspaceCleanupError, CleanupReason,
    trigger_cleanup_on_task_terminal, trigger_cleanup_on_merge,
    DEFAULT_IDLE_THRESHOLD_DAYS,
};
