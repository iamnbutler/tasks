//! Orchestrator for the Tasks platform.
//!
//! The orchestrator is an AI agent that evaluates merge queue entries
//! for quality and provides feedback to implementor agents.
//! See spec §4.2 (evaluation) and §7.4 (conflict triage).
//!
//! Also provides a chat interface for user interaction (orchestrator chat).

pub mod agents;
pub mod chat;
pub mod diff;
pub mod error;
mod claude;
pub mod mock;
mod orchestrator;
mod prompt;
pub mod types;

pub use agents::{AgentConfig, AgentDefinition, built_in_agents, get_agent_definition};
pub use chat::{ChatContext, ChatEvent, ChatResponse, OrchestratorChat, event_to_chat_event};
pub use claude::ClaudeOrchestrator;
pub use error::OrchestratorError;
pub use mock::MockOrchestrator;
pub use orchestrator::Orchestrator;
pub use prompt::parse_pr_url;
pub use types::{
    ConflictContext, ConflictResolution, ConflictTriage, EvaluationContext, OperatingMode,
    OrchestratorAction, QualityEvaluation, QuestionContext, QueueEntrySummary, SystemContext,
};
