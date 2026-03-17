//! LLM agent abstraction layer for the Tasks platform.
//!
//! Provides provider-agnostic types for interacting with LLM APIs.
//! Lifted and adapted from agent-foundry.

pub mod error;
pub mod message;
pub mod provider;
pub mod providers;
pub mod session;

pub use error::AgentError;
pub use message::{Content, Message, Response, Role, StopReason, Tool, ToolCall, ToolResult, Usage};
pub use provider::{
    CompletionConfig, CompletionRequest, Provider, StreamChunk, streaming_from_complete,
};
pub use providers::AnthropicProvider;
pub use session::{Chain, Session, SessionBuilder, SessionId, SessionState};

/// Convenience result type.
pub type Result<T> = std::result::Result<T, AgentError>;
