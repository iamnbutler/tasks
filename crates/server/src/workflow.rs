//! Workflow configuration — spec §14.
//!
//! Reads `workflow.toml` from the project repository root.

use serde::Deserialize;

/// Top-level workflow configuration (spec §14.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorkflowConfig {
    pub project: ProjectConfig,
    pub dispatch: DispatchConfig,
    pub labels: LabelConfig,
    pub prompt: PromptConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    /// Per-project concurrency limit (spec §12.4).
    pub max_sessions: Option<u32>,
    /// Override project default branch.
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DispatchConfig {
    /// Task retry limit (spec §13.2). Default: 3.
    pub max_retries: u32,
    /// Base backoff delay in seconds (spec §13.2). Default: 5.
    pub retry_base_delay: u64,
    /// Minimum runtime (seconds) to count as "progress" (spec §13.1). Default: 60.
    pub progress_threshold: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LabelConfig {
    /// Issues with these labels are not imported (spec §14.2).
    pub ignore: Vec<String>,
    /// Issues with these labels start in blocked state (spec §14.2).
    pub blocked: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PromptConfig {
    /// Path to system prompt file, relative to repo root (spec §14.1).
    pub system_prompt: Option<String>,
}

// --- Default impls ---

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            project: ProjectConfig::default(),
            dispatch: DispatchConfig::default(),
            labels: LabelConfig::default(),
            prompt: PromptConfig::default(),
        }
    }
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            max_sessions: None,
            default_branch: None,
        }
    }
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_base_delay: 5,
            progress_threshold: 60,
        }
    }
}

impl Default for LabelConfig {
    fn default() -> Self {
        Self {
            ignore: vec!["agentic-workflows".to_string()],
            blocked: Vec::new(),
        }
    }
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
        }
    }
}

impl WorkflowConfig {
    /// Parse a TOML string into a `WorkflowConfig`.
    pub fn parse(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let cfg = WorkflowConfig::parse("").unwrap();

        assert!(cfg.project.max_sessions.is_none());
        assert!(cfg.project.default_branch.is_none());
        assert_eq!(cfg.dispatch.max_retries, 3);
        assert_eq!(cfg.dispatch.retry_base_delay, 5);
        assert_eq!(cfg.dispatch.progress_threshold, 60);
        assert_eq!(cfg.labels.ignore, vec!["agentic-workflows"]);
        assert!(cfg.labels.blocked.is_empty());
        assert!(cfg.prompt.system_prompt.is_none());
    }

    #[test]
    fn full_config_parses() {
        let toml_str = r#"
[project]
max_sessions = 8
default_branch = "develop"

[dispatch]
max_retries = 5
retry_base_delay = 10
progress_threshold = 120

[labels]
ignore = ["wontfix", "duplicate"]
blocked = ["needs-design", "waiting-on-upstream"]

[prompt]
system_prompt = ".tasks/prompt.md"
"#;
        let cfg = WorkflowConfig::parse(toml_str).unwrap();

        assert_eq!(cfg.project.max_sessions, Some(8));
        assert_eq!(cfg.project.default_branch.as_deref(), Some("develop"));
        assert_eq!(cfg.dispatch.max_retries, 5);
        assert_eq!(cfg.dispatch.retry_base_delay, 10);
        assert_eq!(cfg.dispatch.progress_threshold, 120);
        assert_eq!(cfg.labels.ignore, vec!["wontfix", "duplicate"]);
        assert_eq!(
            cfg.labels.blocked,
            vec!["needs-design", "waiting-on-upstream"]
        );
        assert_eq!(
            cfg.prompt.system_prompt.as_deref(),
            Some(".tasks/prompt.md")
        );
    }

    #[test]
    fn partial_config_fills_defaults() {
        let toml_str = r#"
[project]
max_sessions = 4

[labels]
ignore = ["spam"]
"#;
        let cfg = WorkflowConfig::parse(toml_str).unwrap();

        // Explicitly set values
        assert_eq!(cfg.project.max_sessions, Some(4));
        assert_eq!(cfg.labels.ignore, vec!["spam"]);

        // Defaults fill in for the rest
        assert!(cfg.project.default_branch.is_none());
        assert_eq!(cfg.dispatch.max_retries, 3);
        assert_eq!(cfg.dispatch.retry_base_delay, 5);
        assert_eq!(cfg.dispatch.progress_threshold, 60);
        assert!(cfg.labels.blocked.is_empty());
        assert!(cfg.prompt.system_prompt.is_none());
    }
}
