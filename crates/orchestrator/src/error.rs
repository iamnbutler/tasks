//! Error types for the orchestrator.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum OrchestratorError {
    #[error("Evaluation failed: {0}")]
    Evaluation(String),

    #[error("Feedback delivery failed: {0}")]
    Feedback(String),

    #[error("GitHub error: {0}")]
    GitHub(String),

    #[error("Agent error: {0}")]
    Agent(#[from] tasks_agent::AgentError),

    #[error("{0}")]
    Other(String),
}
