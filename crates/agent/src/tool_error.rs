//! Granular error variants for tool execution.
//!
//! These replace the catch-all `AgentError::ToolExecution(String)` with typed
//! variants so callers can recover, retry, or surface structured diagnostics
//! to the model (e.g. the difference between "no match" and "too many matches"
//! lets the model respond with more surrounding context instead of guessing).

use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

/// Errors produced by tool implementations.
///
/// Convert into [`crate::AgentError`] via `?` or `.into()` when a tool returns
/// an error up to the session layer.
#[derive(Error, Debug)]
pub enum ToolError {
    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Execution failed: {0}")]
    Execution(String),

    #[error("Timeout after {0:?}")]
    Timeout(Duration),

    #[error("Cancelled")]
    Cancelled,

    #[error("File not found: {0}")]
    FileNotFound(PathBuf),

    #[error("File was modified since last read: {0}")]
    FileModified(PathBuf),

    #[error("String not found in {file}: {snippet}")]
    StringNotFound { file: PathBuf, snippet: String },

    #[error("Multiple matches found in {file} (found {count}): {snippet}")]
    MultipleMatches {
        file: PathBuf,
        snippet: String,
        count: usize,
    },

    #[error("Invalid regex pattern: {0}")]
    InvalidRegex(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl ToolError {
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }

    pub fn permission_denied(msg: impl Into<String>) -> Self {
        Self::PermissionDenied(msg.into())
    }

    pub fn execution(msg: impl Into<String>) -> Self {
        Self::Execution(msg.into())
    }

    pub fn invalid_regex(msg: impl Into<String>) -> Self {
        Self::InvalidRegex(msg.into())
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout(_))
    }

    pub fn is_validation(&self) -> bool {
        matches!(self, Self::Validation(_))
    }

    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::PermissionDenied(_))
    }

    pub fn is_file_error(&self) -> bool {
        matches!(self, Self::FileNotFound(_) | Self::FileModified(_))
    }

    /// True for errors likely to succeed on retry without caller intervention
    /// (transient IO, timeouts, etc.). Validation and permission errors are not retryable.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Timeout(_) | Self::Io(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_not_found_display_includes_file_and_snippet() {
        let err = ToolError::StringNotFound {
            file: PathBuf::from("/tmp/foo.rs"),
            snippet: "fn bar".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/foo.rs"));
        assert!(msg.contains("fn bar"));
    }

    #[test]
    fn multiple_matches_display_includes_count() {
        let err = ToolError::MultipleMatches {
            file: PathBuf::from("src/lib.rs"),
            snippet: "fn new(".into(),
            count: 3,
        };
        let msg = err.to_string();
        assert!(msg.contains("src/lib.rs"));
        assert!(msg.contains("3"));
        assert!(msg.contains("fn new("));
    }

    #[test]
    fn predicates_match_expected_variants() {
        assert!(ToolError::Cancelled.is_cancelled());
        assert!(!ToolError::Cancelled.is_timeout());

        assert!(ToolError::Timeout(Duration::from_secs(1)).is_timeout());
        assert!(ToolError::Validation("nope".into()).is_validation());
        assert!(ToolError::PermissionDenied("no".into()).is_permission_denied());

        assert!(ToolError::FileNotFound(PathBuf::from("/x")).is_file_error());
        assert!(ToolError::FileModified(PathBuf::from("/x")).is_file_error());
        assert!(!ToolError::Validation("x".into()).is_file_error());
    }

    #[test]
    fn retryable_covers_transient_failures_only() {
        assert!(ToolError::Timeout(Duration::from_secs(1)).is_retryable());
        assert!(!ToolError::Validation("bad".into()).is_retryable());
        assert!(!ToolError::PermissionDenied("no".into()).is_retryable());
        assert!(!ToolError::Cancelled.is_retryable());
        assert!(
            !ToolError::StringNotFound {
                file: PathBuf::from("/x"),
                snippet: "s".into(),
            }
            .is_retryable()
        );
    }

    #[test]
    fn io_errors_convert_into_tool_error() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let tool: ToolError = io.into();
        assert!(matches!(tool, ToolError::Io(_)));
        assert!(tool.is_retryable()); // Io variant is retryable by policy
    }

    #[test]
    fn json_errors_convert_into_tool_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("{ invalid").unwrap_err();
        let tool: ToolError = json_err.into();
        assert!(matches!(tool, ToolError::Json(_)));
    }
}
