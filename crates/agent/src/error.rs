//! Error types for the agent crate.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("Provider error: {0}")]
    Provider(String),

    #[error("API error: {message} (status: {status})")]
    Api { status: u16, message: String },

    #[error("Rate limited, retry after {retry_after:?} seconds")]
    RateLimited { retry_after: Option<u64> },

    #[error("Authentication failed: {0}")]
    Auth(String),

    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Session already exists: {0}")]
    SessionExists(String),

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("Tool execution failed: {0}")]
    ToolExecution(String),

    #[error(transparent)]
    Tool(crate::tool_error::ToolError),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Cancelled")]
    Cancelled,

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl AgentError {
    pub fn provider(msg: impl Into<String>) -> Self {
        AgentError::Provider(msg.into())
    }

    pub fn api(status: u16, message: impl Into<String>) -> Self {
        AgentError::Api {
            status,
            message: message.into(),
        }
    }

    pub fn invalid_request(msg: impl Into<String>) -> Self {
        AgentError::InvalidRequest(msg.into())
    }

    /// Returns true if this error is transient and the request may succeed on retry.
    pub fn is_retryable(&self) -> bool {
        match self {
            AgentError::RateLimited { .. } => true,
            AgentError::Network(_) => true,
            AgentError::Api { status, .. } => matches!(status, 500 | 502 | 503 | 529),
            AgentError::Tool(t) => t.is_retryable(),
            _ => false,
        }
    }
}

impl From<crate::tool_error::ToolError> for AgentError {
    fn from(err: crate::tool_error::ToolError) -> Self {
        AgentError::Tool(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_is_retryable() {
        let err = AgentError::RateLimited { retry_after: Some(5) };
        assert!(err.is_retryable());
    }

    #[test]
    fn server_errors_are_retryable() {
        for status in [500, 502, 503, 529] {
            let err = AgentError::api(status, "server error");
            assert!(err.is_retryable(), "status {} should be retryable", status);
        }
    }

    #[test]
    fn client_errors_are_not_retryable() {
        for status in [400, 401, 403, 404, 422] {
            let err = AgentError::api(status, "client error");
            assert!(!err.is_retryable(), "status {} should not be retryable", status);
        }
    }

    #[test]
    fn auth_error_is_not_retryable() {
        let err = AgentError::Auth("bad key".into());
        assert!(!err.is_retryable());
    }

    #[test]
    fn cancelled_is_not_retryable() {
        assert!(!AgentError::Cancelled.is_retryable());
    }

    #[test]
    fn tool_timeout_routes_through_agent_error_retryable() {
        let err: AgentError = crate::tool_error::ToolError::Timeout(
            std::time::Duration::from_secs(1),
        )
        .into();
        assert!(matches!(err, AgentError::Tool(_)));
        assert!(err.is_retryable());
    }

    #[test]
    fn tool_validation_not_retryable_through_agent_error() {
        let err: AgentError =
            crate::tool_error::ToolError::Validation("bad input".into()).into();
        assert!(!err.is_retryable());
    }
}
