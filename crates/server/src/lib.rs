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
pub mod workflow_watcher;
pub mod dispatcher;
pub mod scheduler;
pub mod workspace;
pub mod automation_executor;
mod server;

pub use mode::Mode;
pub use recovery::{RecoveryResult, DEFAULT_MAX_RETRIES};
pub use server::{Server, ServerError, ServerState, RebuildStats};
pub use workflow_watcher::{WorkflowConfigCache, WorkflowConfigWatcher, RefreshResult};
pub use workspace::{
    CleanupCandidate, CleanupConfig, CleanupReason,
    find_cleanup_candidates, is_stale_cleanup, is_terminal_state_cleanup,
    DEFAULT_CLEANUP_INTERVAL_SECS, DEFAULT_CONFLICT_MAX_AGE_SECS, DEFAULT_STALE_THRESHOLD_SECS,
};
