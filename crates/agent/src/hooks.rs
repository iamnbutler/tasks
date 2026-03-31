//! Hook system for tool execution lifecycle.
//!
//! Hooks allow customizing behavior at various points in the tool execution
//! lifecycle: before execution (can block or modify input), after successful
//! execution (can modify output or add context), and after failed execution.
//!
//! Inspired by Claude Code's hook system (`services/tools/toolHooks.ts`).

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::message::ToolCall;

/// Context available to hooks during tool execution.
#[derive(Debug, Clone)]
pub struct ToolUseContext {
    /// Session ID where the tool is being executed.
    pub session_id: String,
    /// All tool names available in the session.
    pub available_tools: Vec<String>,
    /// Arbitrary metadata for hook-specific context.
    pub metadata: serde_json::Value,
}

impl Default for ToolUseContext {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            available_tools: Vec::new(),
            metadata: serde_json::Value::Null,
        }
    }
}

/// Result of a pre-tool-use hook.
#[derive(Debug, Clone)]
pub enum PreHookAction {
    /// Continue execution, optionally with modified input.
    Continue(serde_json::Value),
    /// Block execution with an error message.
    Block(String),
}

/// Result of a post-tool-use hook.
#[derive(Debug, Clone)]
pub enum PostHookAction {
    /// Continue with the output (possibly modified).
    Continue(String),
    /// Continue with output and additional context for the model.
    AddContext {
        output: String,
        context: String,
    },
    /// Stop the query loop after returning this output.
    StopLoop(String),
}

/// Result of a post-tool-use-failure hook.
#[derive(Debug, Clone)]
pub enum FailureHookAction {
    /// Continue with the original error.
    Continue,
    /// Replace the error message.
    ReplaceError(String),
    /// Retry the tool call (with potentially modified input).
    Retry(serde_json::Value),
}

/// Configuration for which tools a hook applies to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookScope {
    /// Applies to all tools.
    All,
    /// Applies only to the named tools.
    Tools(Vec<String>),
    /// Applies to all tools except the named ones.
    Except(Vec<String>),
}

impl HookScope {
    /// Check if this scope matches a given tool name.
    pub fn matches(&self, tool_name: &str) -> bool {
        match self {
            HookScope::All => true,
            HookScope::Tools(names) => names.iter().any(|n| n == tool_name),
            HookScope::Except(names) => !names.iter().any(|n| n == tool_name),
        }
    }
}

/// Trait for tool execution lifecycle hooks.
///
/// Implement this trait to customize tool execution behavior. All methods
/// have default implementations that pass through without modification.
///
/// Hooks are called in registration order. For pre-hooks, if any hook blocks
/// execution, subsequent hooks are not called. For post-hooks, each hook
/// receives the output from the previous hook.
pub trait ToolHook: Send + Sync + fmt::Debug {
    /// Hook name for logging and identification.
    fn name(&self) -> &str;

    /// Which tools this hook applies to.
    fn scope(&self) -> HookScope {
        HookScope::All
    }

    /// Called before tool execution. Can block or modify input.
    fn pre_tool_use(
        &self,
        tool_call: &ToolCall,
        context: &ToolUseContext,
    ) -> PreHookAction {
        let _ = context;
        PreHookAction::Continue(tool_call.arguments.clone())
    }

    /// Called after successful tool execution. Can modify output or add context.
    fn post_tool_use(
        &self,
        tool_call: &ToolCall,
        output: &str,
        context: &ToolUseContext,
    ) -> PostHookAction {
        let _ = (tool_call, context);
        PostHookAction::Continue(output.to_string())
    }

    /// Called when tool execution fails. Can modify the error or request retry.
    fn post_tool_use_failure(
        &self,
        tool_call: &ToolCall,
        error: &str,
        context: &ToolUseContext,
    ) -> FailureHookAction {
        let _ = (tool_call, context);
        let _ = error;
        FailureHookAction::Continue
    }
}

/// Manages an ordered collection of tool hooks.
#[derive(Debug, Clone, Default)]
pub struct HookRegistry {
    hooks: Vec<Arc<dyn ToolHook>>,
}

impl HookRegistry {
    /// Create an empty hook registry.
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Add a hook to the registry.
    pub fn add(&mut self, hook: Arc<dyn ToolHook>) {
        self.hooks.push(hook);
    }

    /// Remove all hooks with the given name.
    pub fn remove(&mut self, name: &str) {
        self.hooks.retain(|h| h.name() != name);
    }

    /// Returns true if the registry has no hooks.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Number of registered hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Run pre-tool-use hooks in order. Returns the (possibly modified) input
    /// or a block error.
    pub fn run_pre_hooks(
        &self,
        tool_call: &ToolCall,
        context: &ToolUseContext,
    ) -> PreHookAction {
        let mut current_input = tool_call.arguments.clone();

        for hook in &self.hooks {
            if !hook.scope().matches(&tool_call.name) {
                continue;
            }

            let call_with_input = ToolCall {
                id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                arguments: current_input.clone(),
            };

            match hook.pre_tool_use(&call_with_input, context) {
                PreHookAction::Continue(new_input) => {
                    current_input = new_input;
                }
                PreHookAction::Block(reason) => {
                    tracing::info!(
                        hook = hook.name(),
                        tool = %tool_call.name,
                        reason = %reason,
                        "tool call blocked by pre-hook"
                    );
                    return PreHookAction::Block(reason);
                }
            }
        }

        PreHookAction::Continue(current_input)
    }

    /// Run post-tool-use hooks in order. Returns the (possibly modified) output
    /// and any additional context.
    pub fn run_post_hooks(
        &self,
        tool_call: &ToolCall,
        output: &str,
        context: &ToolUseContext,
    ) -> PostHookAction {
        let mut current_output = output.to_string();
        let mut accumulated_context: Option<String> = None;
        let mut stop_loop = false;

        for hook in &self.hooks {
            if !hook.scope().matches(&tool_call.name) {
                continue;
            }

            match hook.post_tool_use(tool_call, &current_output, context) {
                PostHookAction::Continue(new_output) => {
                    current_output = new_output;
                }
                PostHookAction::AddContext { output, context: ctx } => {
                    current_output = output;
                    match &mut accumulated_context {
                        Some(existing) => {
                            existing.push('\n');
                            existing.push_str(&ctx);
                        }
                        None => accumulated_context = Some(ctx),
                    }
                }
                PostHookAction::StopLoop(output) => {
                    current_output = output;
                    stop_loop = true;
                }
            }
        }

        if stop_loop {
            PostHookAction::StopLoop(current_output)
        } else if let Some(ctx) = accumulated_context {
            PostHookAction::AddContext {
                output: current_output,
                context: ctx,
            }
        } else {
            PostHookAction::Continue(current_output)
        }
    }

    /// Run post-tool-use-failure hooks in order.
    pub fn run_failure_hooks(
        &self,
        tool_call: &ToolCall,
        error: &str,
        context: &ToolUseContext,
    ) -> FailureHookAction {
        let mut current_error = error.to_string();

        for hook in &self.hooks {
            if !hook.scope().matches(&tool_call.name) {
                continue;
            }

            match hook.post_tool_use_failure(tool_call, &current_error, context) {
                FailureHookAction::Continue => {}
                FailureHookAction::ReplaceError(new_error) => {
                    current_error = new_error;
                }
                FailureHookAction::Retry(input) => {
                    tracing::info!(
                        hook = hook.name(),
                        tool = %tool_call.name,
                        "failure hook requested retry"
                    );
                    return FailureHookAction::Retry(input);
                }
            }
        }

        if current_error != error {
            FailureHookAction::ReplaceError(current_error)
        } else {
            FailureHookAction::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple logging hook for tests.
    #[derive(Debug)]
    struct LoggingHook {
        name: String,
    }

    impl LoggingHook {
        fn new(name: &str) -> Self {
            Self { name: name.to_string() }
        }
    }

    impl ToolHook for LoggingHook {
        fn name(&self) -> &str {
            &self.name
        }
    }

    /// A hook that blocks specific tools.
    #[derive(Debug)]
    struct BlockingHook {
        blocked_tools: Vec<String>,
        reason: String,
    }

    impl ToolHook for BlockingHook {
        fn name(&self) -> &str {
            "blocking"
        }

        fn scope(&self) -> HookScope {
            HookScope::Tools(self.blocked_tools.clone())
        }

        fn pre_tool_use(
            &self,
            _tool_call: &ToolCall,
            _context: &ToolUseContext,
        ) -> PreHookAction {
            PreHookAction::Block(self.reason.clone())
        }
    }

    /// A hook that modifies tool input.
    #[derive(Debug)]
    struct InputModifyHook;

    impl ToolHook for InputModifyHook {
        fn name(&self) -> &str {
            "input-modify"
        }

        fn pre_tool_use(
            &self,
            tool_call: &ToolCall,
            _context: &ToolUseContext,
        ) -> PreHookAction {
            let mut input = tool_call.arguments.clone();
            if let Some(obj) = input.as_object_mut() {
                obj.insert("injected".to_string(), serde_json::json!(true));
            }
            PreHookAction::Continue(input)
        }
    }

    /// A hook that adds context to output.
    #[derive(Debug)]
    struct ContextHook {
        context: String,
    }

    impl ToolHook for ContextHook {
        fn name(&self) -> &str {
            "context-adder"
        }

        fn post_tool_use(
            &self,
            _tool_call: &ToolCall,
            output: &str,
            _context: &ToolUseContext,
        ) -> PostHookAction {
            PostHookAction::AddContext {
                output: output.to_string(),
                context: self.context.clone(),
            }
        }
    }

    fn make_tool_call(name: &str) -> ToolCall {
        ToolCall {
            id: "tc-1".to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({"key": "value"}),
        }
    }

    fn default_context() -> ToolUseContext {
        ToolUseContext::default()
    }

    // --- HookScope tests ---

    #[test]
    fn test_scope_all_matches_everything() {
        assert!(HookScope::All.matches("anything"));
        assert!(HookScope::All.matches("read_file"));
    }

    #[test]
    fn test_scope_tools_matches_listed() {
        let scope = HookScope::Tools(vec!["read".into(), "write".into()]);
        assert!(scope.matches("read"));
        assert!(scope.matches("write"));
        assert!(!scope.matches("bash"));
    }

    #[test]
    fn test_scope_except_excludes_listed() {
        let scope = HookScope::Except(vec!["bash".into()]);
        assert!(scope.matches("read"));
        assert!(!scope.matches("bash"));
    }

    // --- HookRegistry tests ---

    #[test]
    fn test_registry_add_and_remove() {
        let mut registry = HookRegistry::new();
        assert!(registry.is_empty());

        registry.add(Arc::new(LoggingHook::new("hook-a")));
        registry.add(Arc::new(LoggingHook::new("hook-b")));
        assert_eq!(registry.len(), 2);

        registry.remove("hook-a");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_pre_hook_passthrough() {
        let registry = HookRegistry::new();
        let tc = make_tool_call("read");
        let ctx = default_context();

        match registry.run_pre_hooks(&tc, &ctx) {
            PreHookAction::Continue(input) => {
                assert_eq!(input, tc.arguments);
            }
            PreHookAction::Block(_) => panic!("should not block"),
        }
    }

    #[test]
    fn test_pre_hook_blocks() {
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(BlockingHook {
            blocked_tools: vec!["bash".into()],
            reason: "dangerous operation".into(),
        }));

        let tc = make_tool_call("bash");
        let ctx = default_context();

        match registry.run_pre_hooks(&tc, &ctx) {
            PreHookAction::Block(reason) => {
                assert_eq!(reason, "dangerous operation");
            }
            PreHookAction::Continue(_) => panic!("should block"),
        }
    }

    #[test]
    fn test_pre_hook_scope_filtering() {
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(BlockingHook {
            blocked_tools: vec!["bash".into()],
            reason: "blocked".into(),
        }));

        // Non-matching tool should pass through
        let tc = make_tool_call("read");
        let ctx = default_context();

        match registry.run_pre_hooks(&tc, &ctx) {
            PreHookAction::Continue(_) => {}
            PreHookAction::Block(_) => panic!("should not block read"),
        }
    }

    #[test]
    fn test_pre_hook_modifies_input() {
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(InputModifyHook));

        let tc = make_tool_call("read");
        let ctx = default_context();

        match registry.run_pre_hooks(&tc, &ctx) {
            PreHookAction::Continue(input) => {
                assert_eq!(input["injected"], serde_json::json!(true));
                assert_eq!(input["key"], serde_json::json!("value"));
            }
            PreHookAction::Block(_) => panic!("should not block"),
        }
    }

    #[test]
    fn test_pre_hooks_chain_modifications() {
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(InputModifyHook));
        registry.add(Arc::new(InputModifyHook)); // Second hook sees modified input

        let tc = make_tool_call("read");
        let ctx = default_context();

        match registry.run_pre_hooks(&tc, &ctx) {
            PreHookAction::Continue(input) => {
                // Both hooks added "injected": true, so it should be present
                assert_eq!(input["injected"], serde_json::json!(true));
            }
            PreHookAction::Block(_) => panic!("should not block"),
        }
    }

    #[test]
    fn test_post_hook_passthrough() {
        let registry = HookRegistry::new();
        let tc = make_tool_call("read");
        let ctx = default_context();

        match registry.run_post_hooks(&tc, "output", &ctx) {
            PostHookAction::Continue(output) => {
                assert_eq!(output, "output");
            }
            _ => panic!("expected continue"),
        }
    }

    #[test]
    fn test_post_hook_adds_context() {
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(ContextHook {
            context: "extra info".into(),
        }));

        let tc = make_tool_call("read");
        let ctx = default_context();

        match registry.run_post_hooks(&tc, "output", &ctx) {
            PostHookAction::AddContext { output, context } => {
                assert_eq!(output, "output");
                assert_eq!(context, "extra info");
            }
            _ => panic!("expected add context"),
        }
    }

    #[test]
    fn test_failure_hook_passthrough() {
        let registry = HookRegistry::new();
        let tc = make_tool_call("read");
        let ctx = default_context();

        match registry.run_failure_hooks(&tc, "error", &ctx) {
            FailureHookAction::Continue => {}
            _ => panic!("expected continue"),
        }
    }

    #[test]
    fn test_post_hooks_accumulate_context() {
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(ContextHook {
            context: "context A".into(),
        }));
        registry.add(Arc::new(ContextHook {
            context: "context B".into(),
        }));

        let tc = make_tool_call("read");
        let ctx = default_context();

        match registry.run_post_hooks(&tc, "output", &ctx) {
            PostHookAction::AddContext { context, .. } => {
                assert!(context.contains("context A"));
                assert!(context.contains("context B"));
            }
            _ => panic!("expected add context"),
        }
    }
}
