//! LLM agent abstraction layer for the Tasks platform.
//!
//! Provides provider-agnostic types for interacting with LLM APIs.
//! Lifted and adapted from agent-foundry.

pub mod chain;
pub mod completions;
pub mod error;
pub mod hooks;
pub mod message;
pub mod provider;
pub mod providers;
pub mod session;
pub mod tool_exec;

pub use completions::CompletionsService;
pub use error::AgentError;
pub use message::{Content, Message, Response, Role, StopReason, Tool, ToolCall, ToolResult, Usage};
pub use provider::{
    CompletionConfig, CompletionRequest, Provider, StreamChunk, streaming_from_complete,
};
pub use providers::AnthropicProvider;
pub use session::{Chain, Session, SessionBuilder, SessionId, SessionState};
pub use chain::{ChainBuilder, ChainResult, StepOutput, StepResult};
pub use hooks::{
    FailureHookAction, HookRegistry, HookScope, PostHookAction, PreHookAction, ToolHook,
    ToolUseContext,
};
pub use tool_exec::{ToolBatch, execute_tool_calls, execute_tool_calls_with_hooks, partition_tool_calls};

/// Convenience result type.
pub type Result<T> = std::result::Result<T, AgentError>;
