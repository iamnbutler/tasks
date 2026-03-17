//! LLM agent abstraction layer for the Tasks platform.
//!
//! Provides provider-agnostic types for interacting with LLM APIs.
//! Lifted and adapted from agent-foundry.

pub mod error;

pub use error::AgentError;

/// Convenience result type.
pub type Result<T> = std::result::Result<T, AgentError>;
