//! LLM agent abstraction layer for the Tasks platform.
//!
//! Provides provider-agnostic types for interacting with LLM APIs.
//! Lifted and adapted from agent-foundry.

pub mod chain;
pub mod completions;
pub mod error;
pub mod file_state_cache;
pub mod message;
pub mod provider;
pub mod providers;
pub mod session;
pub mod tool_error;
pub mod tool_exec;
pub mod tool_result_budget;

pub use completions::CompletionsService;
pub use error::AgentError;
pub use file_state_cache::{FileState, FileStateCache, SharedFileStateCache};
pub use tool_error::ToolError;
pub use message::{Content, Message, Response, Role, StopReason, Tool, ToolCall, ToolResult, Usage, DEFAULT_MAX_RESULT_SIZE};
pub use provider::{
    CompletionConfig, CompletionRequest, Provider, StreamChunk, streaming_from_complete,
};
pub use providers::AnthropicProvider;
pub use session::{Chain, Session, SessionBuilder, SessionId, SessionState};
pub use chain::{ChainBuilder, ChainResult, StepOutput, StepResult};
pub use tool_exec::{ToolBatch, execute_tool_calls, partition_tool_calls};
pub use tool_result_budget::{budget_tool_result, budget_tool_results, tool_output_dir};

/// Convenience result type.
pub type Result<T> = std::result::Result<T, AgentError>;
