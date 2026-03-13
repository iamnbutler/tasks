//! Project model — spec Section 5.6.

use serde::{Deserialize, Serialize};

/// A project — maps to a single repository.
///
/// Spec Section 5.6, 3.3. The server can manage multiple projects
/// across repos and orgs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    /// Repository reference (owner/repo).
    pub repo: String,
    /// Typically `main`.
    pub default_branch: String,
    /// Project-level configuration.
    pub config: serde_json::Value,
}

impl Project {
    pub fn new(id: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            repo: repo.into(),
            default_branch: "main".to_string(),
            config: serde_json::json!({}),
        }
    }
}
