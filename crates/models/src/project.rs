//! Project model — spec Section 5.6.

use serde::{Deserialize, Serialize};

/// Typed project-level configuration stored in SQLite.
///
/// All fields are optional with sensible defaults so that existing
/// rows containing `{}` deserialize without errors.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProjectConfig {
    /// Maximum retry attempts for failed task sessions.
    pub max_retries: Option<u32>,
    /// Session timeout in seconds.
    pub timeout_seconds: Option<u64>,
    /// Maximum concurrent sessions for this project.
    pub max_sessions: Option<u32>,
}

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
    pub config: ProjectConfig,
}

impl Project {
    pub fn new(id: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            repo: repo.into(),
            default_branch: "main".to_string(),
            config: ProjectConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_json_deserializes_to_defaults() {
        let config: ProjectConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, ProjectConfig::default());
    }

    #[test]
    fn partial_json_deserializes() {
        let config: ProjectConfig =
            serde_json::from_str(r#"{"max_retries": 5}"#).unwrap();
        assert_eq!(config.max_retries, Some(5));
        assert_eq!(config.timeout_seconds, None);
        assert_eq!(config.max_sessions, None);
    }

    #[test]
    fn full_json_roundtrips() {
        let config = ProjectConfig {
            max_retries: Some(3),
            timeout_seconds: Some(600),
            max_sessions: Some(4),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: ProjectConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn default_serializes_to_nulls() {
        let config = ProjectConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // All fields are null (None)
        assert!(parsed["max_retries"].is_null());
        assert!(parsed["timeout_seconds"].is_null());
        assert!(parsed["max_sessions"].is_null());
    }
}
