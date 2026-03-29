//! Orchestrator for the Tasks platform.
//!
//! The orchestrator is an AI agent that evaluates merge queue entries
//! for quality and provides feedback to implementor agents.
//! See spec §4.2 (evaluation) and §7.4 (conflict triage).
//!
//! Also provides a chat interface for user interaction (orchestrator chat).

pub mod chat;
pub mod error;
mod claude;
pub mod mock;
mod orchestrator;
mod prompt;
pub mod types;

pub use chat::{ChatAction, ChatContext, ChatEvent, ChatResponse, OrchestratorChat, event_to_chat_event};
pub use claude::{ClaudeOrchestrator, compute_priority_adjustments};
pub use error::OrchestratorError;
pub use mock::MockOrchestrator;
pub use orchestrator::Orchestrator;
pub use prompt::parse_pr_url;
pub use types::{
    ConflictContext, ConflictResolution, ConflictTriage, EvaluationContext, OperatingMode,
    OrchestratorAction, QualityEvaluation, QuestionContext, QueueEntrySummary, SystemContext,
};
