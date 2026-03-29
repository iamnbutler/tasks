//! Parked question model — spec §4.1 (autonomous mode, issue #534).
//!
//! When the human is disconnected and the orchestrator isn't confident
//! enough to answer an agent's question, it parks the question for
//! the human to address on reconnection.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A question that the orchestrator parked for the human to answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParkedQuestion {
    /// Unique ID.
    pub id: String,
    /// The task whose agent asked the question.
    pub task_id: String,
    /// The question text from the agent.
    pub question: String,
    /// Why the orchestrator parked it instead of answering.
    pub reason: String,
    /// When the question was parked.
    pub parked_at: DateTime<Utc>,
    /// When the question was resolved (None if still pending).
    pub resolved_at: Option<DateTime<Utc>>,
    /// How it was resolved: "answered_by_human", "answered_by_orchestrator", "expired".
    pub resolution: Option<String>,
}

impl ParkedQuestion {
    /// Create a new parked question.
    pub fn new(
        id: impl Into<String>,
        task_id: impl Into<String>,
        question: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            task_id: task_id.into(),
            question: question.into(),
            reason: reason.into(),
            parked_at: Utc::now(),
            resolved_at: None,
            resolution: None,
        }
    }
}
