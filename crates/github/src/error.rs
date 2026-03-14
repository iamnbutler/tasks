//! Error types for the GitHub integration crate.

use chrono::{DateTime, Utc};
use thiserror::Error;

/// Errors returned by the GitHub client.
#[derive(Debug, Error)]
pub enum GitHubError {
    /// Authentication failed (401).
    #[error("authentication failed: {0}")]
    Auth(String),

    /// Resource not found (404 or GraphQL NOT_FOUND).
    #[error("not found: {0}")]
    NotFound(String),

    /// Rate limit exceeded after retry.
    #[error("rate limited, resets at {reset_at}")]
    RateLimited {
        reset_at: DateTime<Utc>,
    },

    /// GitHub returned GraphQL-level errors.
    #[error("GraphQL errors: {0:?}")]
    GraphQL(Vec<GraphQLError>),

    /// Network or HTTP transport error.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// Response did not match expected shape.
    #[error("decode error: {0}")]
    Decode(String),
}

/// A single error from a GraphQL response.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GraphQLError {
    pub message: String,
    #[serde(default)]
    pub path: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub locations: Option<Vec<GraphQLLocation>>,
    #[serde(rename = "type", default)]
    pub error_type: Option<String>,
}

/// Source location within a GraphQL query.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GraphQLLocation {
    pub line: u64,
    pub column: u64,
}
