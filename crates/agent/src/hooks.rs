//! Hook system for tool execution lifecycle.
//!
//! Hooks allow customizing behavior at various points in the tool execution
//! lifecycle: before execution (can block or modify input), after execution
//! (can modify output or add context), and on failure.
//!
//! Inspired by Claude Code's hook system (`services/tools/toolHooks.ts`).

use std::collections::HashMap;
use std::sync::Arc;

use crate::message::{ToolCall, ToolResult};

/// Context provided to hooks during tool execution.
#[derive(Debug, Clone)]
pub struct ToolUseContext {
    /// The session ID where the tool is being executed.
    pub session_id: String,
    /// Metadata from the session (arbitrary key-value pairs).
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Result of a pre-tool-use hook.
#[derive(Debug, Clone)]
pub enum PreHookResult {
    /// Continue execution, possibly with modified input.
    Continue(serde_json::Value),
    /// Block execution with an error message.
    Block(String),
}

/// Result of a post-tool-use hook.
#[derive(Debug, Clone)]
pub enum PostHookResult {
    /// Continue with the (possibly modified) output.
    Continue(String),
    /// Continue with output and add additional context for the model.
    AddContext {
        output: String,
        context: String,
    },
    /// Stop the query loop with this output.
    StopLoop(String),
}

/// Trait for hooks that intercept tool execution.
///
/// Implement this trait to add custom behavior around tool calls.
/// All methods have default implementations that pass through unchanged.
///
/// # Examples
///
/// ```rust
/// use tasks_agent::hooks::{ToolHook, ToolUseContext, PreHookResult, PostHookResult};
///
/// struct LoggingHook;
///
/// impl ToolHook for LoggingHook {
///     fn pre_tool_use(
///         &self,
///         tool_name: &str,
///         input: &serde_json::Value,
///         _context: &ToolUseContext,
///     ) -> PreHookResult {
///         println!("Calling tool: {} with input: {}", tool_name, input);
///         PreHookResult::Continue(input.clone())
///     }
///
///     fn post_tool_use(
///         &self,
///         tool_name: &str,
///         _input: &serde_json::Value,
///         output: &str,
///         _context: &ToolUseContext,
///     ) -> PostHookResult {
///         println!("Tool {} returned: {}", tool_name, &output[..output.len().min(100)]);
///         PostHookResult::Continue(output.to_string())
///     }
/// }
/// ```
pub trait ToolHook: Send + Sync {
    /// Called before tool execution. Can block or modify input.
    fn pre_tool_use(
        &self,
        _tool_name: &str,
        input: &serde_json::Value,
        _context: &ToolUseContext,
    ) -> PreHookResult {
        PreHookResult::Continue(input.clone())
    }

    /// Called after successful tool execution. Can modify output or add context.
    fn post_tool_use(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
        output: &str,
        _context: &ToolUseContext,
    ) -> PostHookResult {
        PostHookResult::Continue(output.to_string())
    }

    /// Called when tool execution fails. Can modify the error or add context.
    fn post_tool_use_failure(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
        error: &str,
        _context: &ToolUseContext,
    ) -> PostHookResult {
        PostHookResult::Continue(error.to_string())
    }
}

/// A collection of hooks that are applied in order.
#[derive(Clone, Default)]
pub struct HookRegistry {
    hooks: Vec<Arc<dyn ToolHook>>,
}

impl std::fmt::Debug for HookRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRegistry")
            .field("count", &self.hooks.len())
            .finish()
    }
}

impl HookRegistry {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Add a hook to the registry. Hooks are called in the order they are added.
    pub fn add(&mut self, hook: Arc<dyn ToolHook>) {
        self.hooks.push(hook);
    }

    /// Returns true if the registry has no hooks.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Returns the number of registered hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Run all pre-tool-use hooks in order. Returns the final input or a block error.
    pub fn run_pre_hooks(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        context: &ToolUseContext,
    ) -> PreHookResult {
        let mut current_input = input.clone();
        for hook in &self.hooks {
            match hook.pre_tool_use(tool_name, &current_input, context) {
                PreHookResult::Continue(new_input) => current_input = new_input,
                PreHookResult::Block(error) => return PreHookResult::Block(error),
            }
        }
        PreHookResult::Continue(current_input)
    }

    /// Run all post-tool-use hooks in order.
    pub fn run_post_hooks(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        output: &str,
        is_error: bool,
        context: &ToolUseContext,
    ) -> PostHookResult {
        let mut current_output = output.to_string();
        for hook in &self.hooks {
            let result = if is_error {
                hook.post_tool_use_failure(tool_name, input, &current_output, context)
            } else {
                hook.post_tool_use(tool_name, input, &current_output, context)
            };
            match result {
                PostHookResult::Continue(new_output) => current_output = new_output,
                PostHookResult::AddContext { output: new_output, context: ctx } => {
                    // Append context to output for subsequent hooks
                    current_output = format!("{}\n\n[Hook context: {}]", new_output, ctx);
                }
                PostHookResult::StopLoop(msg) => return PostHookResult::StopLoop(msg),
            }
        }
        PostHookResult::Continue(current_output)
    }

    /// Apply hooks around a single tool call, returning the modified ToolResult.
    ///
    /// This is the main integration point: it runs pre-hooks, and if not blocked,
    /// delegates to the caller's result, then runs post-hooks.
    pub fn apply_to_result(
        &self,
        tool_call: &ToolCall,
        result: ToolResult,
        context: &ToolUseContext,
    ) -> ToolResult {
        // Post-hooks
        match self.run_post_hooks(
            &tool_call.name,
            &tool_call.arguments,
            &result.content,
            result.is_error,
            context,
        ) {
            PostHookResult::Continue(output) | PostHookResult::StopLoop(output) => {
                ToolResult {
                    content: output,
                    ..result
                }
            }
            PostHookResult::AddContext { output, .. } => {
                ToolResult {
                    content: output,
                    ..result
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BlockDangerousHook;

    impl ToolHook for BlockDangerousHook {
        fn pre_tool_use(
            &self,
            tool_name: &str,
            input: &serde_json::Value,
            _context: &ToolUseContext,
        ) -> PreHookResult {
            if tool_name == "bash" {
                if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                    if cmd.contains("rm -rf /") {
                        return PreHookResult::Block(
                            "Blocked: dangerous command detected".to_string(),
                        );
                    }
                }
            }
            PreHookResult::Continue(input.clone())
        }
    }

    struct LoggingHook {
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ToolHook for LoggingHook {
        fn pre_tool_use(
            &self,
            tool_name: &str,
            input: &serde_json::Value,
            _context: &ToolUseContext,
        ) -> PreHookResult {
            self.log
                .lock()
                .unwrap()
                .push(format!("pre:{}", tool_name));
            PreHookResult::Continue(input.clone())
        }

        fn post_tool_use(
            &self,
            tool_name: &str,
            _input: &serde_json::Value,
            output: &str,
            _context: &ToolUseContext,
        ) -> PostHookResult {
            self.log
                .lock()
                .unwrap()
                .push(format!("post:{}", tool_name));
            PostHookResult::Continue(output.to_string())
        }
    }

    struct InputTransformHook;

    impl ToolHook for InputTransformHook {
        fn pre_tool_use(
            &self,
            _tool_name: &str,
            input: &serde_json::Value,
            _context: &ToolUseContext,
        ) -> PreHookResult {
            // Normalize file paths: expand ~ to /home/user
            let mut modified = input.clone();
            if let Some(path) = modified.get("path").and_then(|v| v.as_str()) {
                if path.starts_with("~/") {
                    modified["path"] =
                        serde_json::Value::String(path.replacen("~/", "/home/user/", 1));
                }
            }
            PreHookResult::Continue(modified)
        }
    }

    struct ContextHook;

    impl ToolHook for ContextHook {
        fn post_tool_use(
            &self,
            _tool_name: &str,
            _input: &serde_json::Value,
            output: &str,
            _context: &ToolUseContext,
        ) -> PostHookResult {
            PostHookResult::AddContext {
                output: output.to_string(),
                context: "File was modified at 2024-01-01".to_string(),
            }
        }
    }

    struct StopLoopHook;

    impl ToolHook for StopLoopHook {
        fn post_tool_use(
            &self,
            tool_name: &str,
            _input: &serde_json::Value,
            output: &str,
            _context: &ToolUseContext,
        ) -> PostHookResult {
            if tool_name == "expensive_tool" {
                return PostHookResult::StopLoop("Rate limit reached".to_string());
            }
            PostHookResult::Continue(output.to_string())
        }
    }

    fn test_context() -> ToolUseContext {
        ToolUseContext {
            session_id: "test-session".to_string(),
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn test_empty_registry_passes_through() {
        let registry = HookRegistry::new();
        let ctx = test_context();
        let input = serde_json::json!({"command": "ls"});

        match registry.run_pre_hooks("bash", &input, &ctx) {
            PreHookResult::Continue(result) => assert_eq!(result, input),
            PreHookResult::Block(_) => panic!("should not block"),
        }
    }

    #[test]
    fn test_pre_hook_blocks_dangerous_command() {
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(BlockDangerousHook));
        let ctx = test_context();

        // Safe command passes
        let safe_input = serde_json::json!({"command": "ls -la"});
        match registry.run_pre_hooks("bash", &safe_input, &ctx) {
            PreHookResult::Continue(_) => {}
            PreHookResult::Block(e) => panic!("should not block: {}", e),
        }

        // Dangerous command is blocked
        let dangerous_input = serde_json::json!({"command": "rm -rf /"});
        match registry.run_pre_hooks("bash", &dangerous_input, &ctx) {
            PreHookResult::Block(msg) => {
                assert!(msg.contains("dangerous command"));
            }
            PreHookResult::Continue(_) => panic!("should have blocked"),
        }
    }

    #[test]
    fn test_pre_hook_transforms_input() {
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(InputTransformHook));
        let ctx = test_context();

        let input = serde_json::json!({"path": "~/Documents/file.txt"});
        match registry.run_pre_hooks("read_file", &input, &ctx) {
            PreHookResult::Continue(result) => {
                assert_eq!(
                    result.get("path").unwrap().as_str().unwrap(),
                    "/home/user/Documents/file.txt"
                );
            }
            PreHookResult::Block(_) => panic!("should not block"),
        }
    }

    #[test]
    fn test_logging_hook_records_calls() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(LoggingHook { log: log.clone() }));
        let ctx = test_context();

        let input = serde_json::json!({});
        registry.run_pre_hooks("read_file", &input, &ctx);
        registry.run_post_hooks("read_file", &input, "contents", false, &ctx);

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], "pre:read_file");
        assert_eq!(entries[1], "post:read_file");
    }

    #[test]
    fn test_post_hook_add_context() {
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(ContextHook));
        let ctx = test_context();
        let input = serde_json::json!({});

        match registry.run_post_hooks("write_file", &input, "ok", false, &ctx) {
            PostHookResult::Continue(output) => {
                assert!(output.contains("ok"));
                assert!(output.contains("File was modified"));
            }
            _ => panic!("expected Continue with context appended"),
        }
    }

    #[test]
    fn test_post_hook_stop_loop() {
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(StopLoopHook));
        let ctx = test_context();
        let input = serde_json::json!({});

        match registry.run_post_hooks("expensive_tool", &input, "result", false, &ctx) {
            PostHookResult::StopLoop(msg) => assert_eq!(msg, "Rate limit reached"),
            _ => panic!("expected StopLoop"),
        }

        // Non-matching tool passes through
        match registry.run_post_hooks("cheap_tool", &input, "result", false, &ctx) {
            PostHookResult::Continue(output) => assert_eq!(output, "result"),
            _ => panic!("expected Continue"),
        }
    }

    #[test]
    fn test_multiple_hooks_chain() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(InputTransformHook));
        registry.add(Arc::new(LoggingHook { log: log.clone() }));
        registry.add(Arc::new(BlockDangerousHook));
        let ctx = test_context();

        // All hooks run in order
        let input = serde_json::json!({"path": "~/file.txt"});
        match registry.run_pre_hooks("read_file", &input, &ctx) {
            PreHookResult::Continue(result) => {
                assert_eq!(
                    result.get("path").unwrap().as_str().unwrap(),
                    "/home/user/file.txt"
                );
            }
            PreHookResult::Block(_) => panic!("should not block"),
        }

        let entries = log.lock().unwrap();
        assert_eq!(entries[0], "pre:read_file");
    }

    #[test]
    fn test_block_stops_chain() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(BlockDangerousHook));
        registry.add(Arc::new(LoggingHook { log: log.clone() }));
        let ctx = test_context();

        let input = serde_json::json!({"command": "rm -rf /"});
        match registry.run_pre_hooks("bash", &input, &ctx) {
            PreHookResult::Block(_) => {}
            PreHookResult::Continue(_) => panic!("should have blocked"),
        }

        // Logging hook should NOT have been called (block stops chain)
        let entries = log.lock().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_apply_to_result() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = HookRegistry::new();
        registry.add(Arc::new(LoggingHook { log: log.clone() }));
        let ctx = test_context();

        let tool_call = ToolCall {
            id: "tc-1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "/tmp/test"}),
        };
        let result = ToolResult::success("tc-1", "file contents");

        let hooked_result = registry.apply_to_result(&tool_call, result, &ctx);
        assert_eq!(hooked_result.content, "file contents");
        assert!(!hooked_result.is_error);

        let entries = log.lock().unwrap();
        assert_eq!(entries[0], "post:read_file");
    }

    #[test]
    fn test_failure_hook() {
        struct FailureTransformHook;
        impl ToolHook for FailureTransformHook {
            fn post_tool_use_failure(
                &self,
                _tool_name: &str,
                _input: &serde_json::Value,
                error: &str,
                _context: &ToolUseContext,
            ) -> PostHookResult {
                PostHookResult::Continue(format!("Wrapped error: {}", error))
            }
        }

        let mut registry = HookRegistry::new();
        registry.add(Arc::new(FailureTransformHook));
        let ctx = test_context();

        let tool_call = ToolCall {
            id: "tc-1".to_string(),
            name: "bash".to_string(),
            arguments: serde_json::json!({}),
        };
        let result = ToolResult::error("tc-1", "command failed");

        let hooked = registry.apply_to_result(&tool_call, result, &ctx);
        assert!(hooked.content.contains("Wrapped error: command failed"));
    }

    #[test]
    fn test_registry_len_and_is_empty() {
        let mut registry = HookRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);

        registry.add(Arc::new(BlockDangerousHook));
        assert!(!registry.is_empty());
        assert_eq!(registry.len(), 1);
    }
}
