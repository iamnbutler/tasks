//! Orchestrator for the Tasks platform.
//!
//! The orchestrator is an AI agent that evaluates merge queue entries
//! for quality and provides feedback to implementor agents.
//! See spec §4.2.

pub mod error;
mod claude;
pub mod mock;
mod orchestrator;
mod prompt;
pub mod types;

pub use claude::ClaudeOrchestrator;
pub use error::OrchestratorError;
pub use mock::MockOrchestrator;
pub use orchestrator::Orchestrator;
pub use prompt::parse_pr_url;
pub use types::{EvaluationContext, QualityEvaluation};
