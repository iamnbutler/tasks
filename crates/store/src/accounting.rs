//! Task accounting storage — spec §16.4.
//!
//! Stores cumulative token usage and session statistics per task.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Accounting data for a single task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAccounting {
    /// Task ID this accounting data belongs to.
    pub task_id: String,
    /// Total input tokens used across all sessions.
    pub total_input_tokens: u64,
    /// Total output tokens used across all sessions.
    pub total_output_tokens: u64,
    /// Number of sessions run for this task.
    pub session_count: u32,
    /// Total wall-clock seconds across all sessions.
    pub total_duration_seconds: u64,
    /// Last time this record was updated.
    pub last_updated: DateTime<Utc>,
}

impl TaskAccounting {
    /// Create a new accounting record for a task.
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            session_count: 0,
            total_duration_seconds: 0,
            last_updated: Utc::now(),
        }
    }

    /// Total tokens used (input + output).
    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens
    }

    /// Add token usage from a session.
    pub fn add_tokens(&mut self, input: u64, output: u64) {
        self.total_input_tokens += input;
        self.total_output_tokens += output;
        self.last_updated = Utc::now();
    }

    /// Record a session completion with its duration.
    pub fn record_session(&mut self, duration_seconds: u64) {
        self.session_count += 1;
        self.total_duration_seconds += duration_seconds;
        self.last_updated = Utc::now();
    }
}

/// Global accounting summary across all tasks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountingSummary {
    /// Total input tokens across all tasks.
    pub total_input_tokens: u64,
    /// Total output tokens across all tasks.
    pub total_output_tokens: u64,
    /// Total sessions across all tasks.
    pub total_sessions: u32,
    /// Total runtime seconds across all tasks.
    pub total_duration_seconds: u64,
    /// Number of tasks with accounting data.
    pub task_count: u32,
}

impl AccountingSummary {
    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens
    }
}
