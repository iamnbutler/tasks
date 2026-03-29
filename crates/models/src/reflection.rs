//! Reflection model — spec Section 8.
//!
//! Reflections are post-merge review issues that provide asynchronous human
//! feedback on merged changes. They are GitHub issues with a configurable
//! label (default: "reflection").

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// State of a reflection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReflectionState {
    /// Reflection is open and accepting feedback.
    Open,
    /// Reflection has been resolved/closed.
    Closed,
}

/// A reflection comment from a GitHub issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflectionComment {
    /// Comment author login.
    pub author: String,
    /// Comment body text.
    pub body: String,
    /// When the comment was created.
    pub created_at: DateTime<Utc>,
}

/// A reflection — a post-merge review issue (spec §8.2).
///
/// Reflections are advisory and non-blocking. They allow humans to provide
/// feedback on merged changes that the orchestrator can reference during
/// future PR evaluations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reflection {
    /// Unique ID: `reflection-{owner}-{repo}-{number}`.
    pub id: String,
    /// GitHub issue number.
    pub number: u64,
    /// Repository owner.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Reflection title.
    pub title: String,
    /// Reflection body (markdown).
    pub body: Option<String>,
    /// Current state.
    pub state: ReflectionState,
    /// Labels on the issue.
    pub labels: Vec<String>,
    /// Comments on the reflection.
    pub comments: Vec<ReflectionComment>,
    /// Project ID this reflection belongs to.
    pub project: String,
    /// GitHub issue URL.
    pub url: String,
    /// When the reflection was created.
    pub created_at: DateTime<Utc>,
    /// When the reflection was last updated.
    pub updated_at: DateTime<Utc>,
    /// When the reflection was closed (if closed).
    pub closed_at: Option<DateTime<Utc>>,
}

impl Reflection {
    /// Build the canonical ID for a reflection.
    pub fn make_id(owner: &str, repo: &str, number: u64) -> String {
        format!("reflection-{}-{}-{}", owner, repo, number)
    }

    /// Build the GitHub issue URL.
    pub fn make_url(owner: &str, repo: &str, number: u64) -> String {
        format!("https://github.com/{}/{}/issues/{}", owner, repo, number)
    }
}
