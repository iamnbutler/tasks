//! LLM agent abstraction layer for the Tasks platform.
//!
//! Provides provider-agnostic types for interacting with LLM APIs.
//! Lifted and adapted from agent-foundry.

pub mod error;
pub mod message;

pub use error::AgentError;
pub use message::{Content, Message, Response, Role, StopReason, Tool, ToolCall, ToolResult, Usage};

/// Convenience result type.
pub type Result<T> = std::result::Result<T, AgentError>;
