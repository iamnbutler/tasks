//! Agent type definitions for specialized container agents.
//!
//! Each agent type has specific tool restrictions, model preferences, and
//! behavioral limits. This allows the orchestrator to dispatch the right
//! kind of agent for each task — implementers for coding, reviewers for
//! read-only quality checks, explorers for codebase research.
//!
//! Inspired by Claude Code's AgentDefinition system (tools/AgentTool).

use serde::{Deserialize, Serialize};

/// Definition of a specialized agent type.
///
/// Agent types control what tools an agent can use, which model it runs on,
/// and how many turns it gets. Tool restrictions are enforced by the
/// container supervisor when starting the agent process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// Unique identifier for this agent type (e.g., "implementer", "reviewer").
    pub agent_type: String,

    /// Human-readable description of when to use this agent type.
    /// Used by the orchestrator to select the right agent for a task.
    pub when_to_use: String,

    /// Tool allowlist. If `Some`, only these tools are available.
    /// If `None`, all tools are available.
    pub allowed_tools: Option<Vec<String>>,

    /// Tool denylist. These tools are explicitly blocked.
    /// Applied after the allowlist (if both are set, denylist wins).
    pub disallowed_tools: Option<Vec<String>>,

    /// Model override. If `None`, uses the system default.
    pub model: Option<String>,

    /// Maximum number of conversation turns before the agent is stopped.
    /// Prevents runaway agents.
    pub max_turns: Option<u32>,

    /// System prompt template for this agent type.
    /// Can reference `{task}`, `{repo}`, `{branch}` placeholders.
    pub system_prompt_template: Option<String>,
}

impl AgentDefinition {
    /// Create a new agent definition with required fields.
    pub fn new(agent_type: impl Into<String>, when_to_use: impl Into<String>) -> Self {
        Self {
            agent_type: agent_type.into(),
            when_to_use: when_to_use.into(),
            allowed_tools: None,
            disallowed_tools: None,
            model: None,
            max_turns: None,
            system_prompt_template: None,
        }
    }

    /// Set the tool allowlist.
    pub fn with_allowed_tools(mut self, tools: Vec<String>) -> Self {
        self.allowed_tools = Some(tools);
        self
    }

    /// Set the tool denylist.
    pub fn with_disallowed_tools(mut self, tools: Vec<String>) -> Self {
        self.disallowed_tools = Some(tools);
        self
    }

    /// Set the model override.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set the maximum number of turns.
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    /// Set the system prompt template.
    pub fn with_system_prompt_template(mut self, template: impl Into<String>) -> Self {
        self.system_prompt_template = Some(template.into());
        self
    }

    /// Check whether a tool is allowed for this agent type.
    pub fn is_tool_allowed(&self, tool_name: &str) -> bool {
        // Denylist takes precedence
        if let Some(ref denied) = self.disallowed_tools {
            if denied.iter().any(|t| t == tool_name) {
                return false;
            }
        }

        // If allowlist is set, tool must be in it
        if let Some(ref allowed) = self.allowed_tools {
            return allowed.iter().any(|t| t == tool_name);
        }

        true
    }
}

/// Configuration passed to a container when starting an agent.
///
/// Extracted from an `AgentDefinition` and sent as part of the `StartCommand`
/// protocol message. The supervisor uses this to configure the agent process.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Agent type identifier (e.g., "implementer", "reviewer").
    pub agent_type: Option<String>,

    /// Tool allowlist — only these tools are available.
    pub allowed_tools: Option<Vec<String>>,

    /// Tool denylist — these tools are blocked.
    pub disallowed_tools: Option<Vec<String>>,

    /// Model to use for this agent.
    pub model: Option<String>,

    /// Maximum conversation turns.
    pub max_turns: Option<u32>,
}

impl AgentConfig {
    /// Create an `AgentConfig` from an `AgentDefinition`.
    pub fn from_definition(def: &AgentDefinition) -> Self {
        Self {
            agent_type: Some(def.agent_type.clone()),
            allowed_tools: def.allowed_tools.clone(),
            disallowed_tools: def.disallowed_tools.clone(),
            model: def.model.clone(),
            max_turns: def.max_turns,
        }
    }
}

/// Returns the built-in agent type definitions.
///
/// These are the default agent types available in the system. Custom agent
/// types can be defined through project configuration in the future.
pub fn built_in_agents() -> Vec<AgentDefinition> {
    vec![
        // Full-capability implementation agent
        AgentDefinition::new(
            "implementer",
            "Implement features, fix bugs, write code, and make changes to the codebase",
        )
        .with_model("claude-sonnet-4-6".to_string())
        .with_max_turns(200),

        // Read-only code reviewer
        AgentDefinition::new(
            "reviewer",
            "Review code changes for quality, correctness, and adherence to conventions",
        )
        .with_allowed_tools(vec![
            "Read".to_string(),
            "Grep".to_string(),
            "Glob".to_string(),
            "Bash".to_string(),
        ])
        .with_disallowed_tools(vec![
            "Edit".to_string(),
            "Write".to_string(),
            "NotebookEdit".to_string(),
        ])
        .with_model("claude-opus-4-6".to_string())
        .with_max_turns(30),

        // Lightweight codebase explorer
        AgentDefinition::new(
            "explorer",
            "Research codebase structure, find patterns, answer questions about the code",
        )
        .with_allowed_tools(vec![
            "Read".to_string(),
            "Grep".to_string(),
            "Glob".to_string(),
        ])
        .with_model("claude-sonnet-4-6".to_string())
        .with_max_turns(20),
    ]
}

/// Look up a built-in agent definition by type name.
pub fn get_agent_definition(agent_type: &str) -> Option<AgentDefinition> {
    built_in_agents()
        .into_iter()
        .find(|a| a.agent_type == agent_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_agents_are_valid() {
        let agents = built_in_agents();
        assert_eq!(agents.len(), 3);

        let types: Vec<&str> = agents.iter().map(|a| a.agent_type.as_str()).collect();
        assert!(types.contains(&"implementer"));
        assert!(types.contains(&"reviewer"));
        assert!(types.contains(&"explorer"));
    }

    #[test]
    fn get_agent_definition_found() {
        let def = get_agent_definition("reviewer").unwrap();
        assert_eq!(def.agent_type, "reviewer");
        assert_eq!(def.model.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn get_agent_definition_not_found() {
        assert!(get_agent_definition("nonexistent").is_none());
    }

    #[test]
    fn tool_restrictions_implementer() {
        let def = get_agent_definition("implementer").unwrap();
        // Implementer has no restrictions
        assert!(def.is_tool_allowed("Edit"));
        assert!(def.is_tool_allowed("Write"));
        assert!(def.is_tool_allowed("Read"));
        assert!(def.is_tool_allowed("Bash"));
    }

    #[test]
    fn tool_restrictions_reviewer() {
        let def = get_agent_definition("reviewer").unwrap();
        // Reviewer can read but not write
        assert!(def.is_tool_allowed("Read"));
        assert!(def.is_tool_allowed("Grep"));
        assert!(def.is_tool_allowed("Glob"));
        assert!(def.is_tool_allowed("Bash"));
        assert!(!def.is_tool_allowed("Edit"));
        assert!(!def.is_tool_allowed("Write"));
        assert!(!def.is_tool_allowed("NotebookEdit"));
    }

    #[test]
    fn tool_restrictions_explorer() {
        let def = get_agent_definition("explorer").unwrap();
        // Explorer is strictly read-only
        assert!(def.is_tool_allowed("Read"));
        assert!(def.is_tool_allowed("Grep"));
        assert!(def.is_tool_allowed("Glob"));
        assert!(!def.is_tool_allowed("Bash"));
        assert!(!def.is_tool_allowed("Edit"));
        assert!(!def.is_tool_allowed("Write"));
    }

    #[test]
    fn denylist_overrides_allowlist() {
        let def = AgentDefinition::new("test", "test")
            .with_allowed_tools(vec!["Edit".to_string(), "Read".to_string()])
            .with_disallowed_tools(vec!["Edit".to_string()]);
        assert!(!def.is_tool_allowed("Edit")); // denied
        assert!(def.is_tool_allowed("Read")); // allowed
        assert!(!def.is_tool_allowed("Write")); // not in allowlist
    }

    #[test]
    fn agent_config_from_definition() {
        let def = get_agent_definition("reviewer").unwrap();
        let config = AgentConfig::from_definition(&def);
        assert_eq!(config.agent_type.as_deref(), Some("reviewer"));
        assert_eq!(config.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(config.max_turns, Some(30));
        assert!(config.allowed_tools.is_some());
        assert!(config.disallowed_tools.is_some());
    }

    #[test]
    fn agent_config_default_is_empty() {
        let config = AgentConfig::default();
        assert!(config.agent_type.is_none());
        assert!(config.allowed_tools.is_none());
        assert!(config.disallowed_tools.is_none());
        assert!(config.model.is_none());
        assert!(config.max_turns.is_none());
    }

    #[test]
    fn agent_definition_serialization_roundtrip() {
        let def = get_agent_definition("implementer").unwrap();
        let json = serde_json::to_string(&def).unwrap();
        let parsed: AgentDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_type, "implementer");
        assert_eq!(parsed.model, def.model);
        assert_eq!(parsed.max_turns, def.max_turns);
    }
}
