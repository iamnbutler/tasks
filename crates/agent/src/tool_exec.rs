//! Tool execution utilities for concurrent and serial tool dispatch.
//!
//! Partitions tool calls by concurrency safety and runs safe tools in parallel
//! (up to a configurable limit) while running unsafe tools serially.

use std::collections::HashMap;
use std::future::Future;

use futures::future::join_all;

use crate::message::{Tool, ToolCall, ToolResult};

/// Maximum number of concurrency-safe tools to run in parallel.
const MAX_CONCURRENT: usize = 10;

/// A batch of tool calls that share the same concurrency mode.
#[derive(Debug)]
pub struct ToolBatch {
    /// Whether the tools in this batch are safe to run concurrently.
    pub is_concurrent: bool,
    /// The tool calls in this batch.
    pub calls: Vec<ToolCall>,
}

/// Partition tool calls into batches that preserve ordering while grouping
/// consecutive concurrency-safe calls together.
///
/// This mirrors the partitioning strategy from Claude Code's `toolOrchestration.ts`:
/// consecutive safe tools are grouped into a single batch for parallel execution,
/// while unsafe tools each get their own serial batch. Order between batches is
/// preserved.
pub fn partition_tool_calls(tool_calls: Vec<ToolCall>, tools: &[Tool]) -> Vec<ToolBatch> {
    let tool_map: HashMap<&str, &Tool> = tools.iter().map(|t| (t.name.as_str(), t)).collect();

    let mut batches: Vec<ToolBatch> = Vec::new();

    for tc in tool_calls {
        let is_safe = tool_map
            .get(tc.name.as_str())
            .map(|t| t.is_concurrency_safe)
            .unwrap_or(false);

        if is_safe {
            if let Some(last) = batches.last_mut() {
                if last.is_concurrent {
                    last.calls.push(tc);
                    continue;
                }
            }
            batches.push(ToolBatch {
                is_concurrent: true,
                calls: vec![tc],
            });
        } else {
            batches.push(ToolBatch {
                is_concurrent: false,
                calls: vec![tc],
            });
        }
    }

    batches
}

/// Execute tool calls with concurrency-safe tools running in parallel.
///
/// The `executor` function handles the actual tool execution. Safe tool batches
/// run concurrently (up to [`MAX_CONCURRENT`]), while unsafe tools run one at a time.
/// Results are returned in the same order as the original tool calls.
pub async fn execute_tool_calls<F, Fut>(
    tool_calls: Vec<ToolCall>,
    tools: &[Tool],
    executor: F,
) -> Vec<ToolResult>
where
    F: Fn(ToolCall) -> Fut + Send + Sync,
    Fut: Future<Output = ToolResult> + Send,
{
    let batches = partition_tool_calls(tool_calls, tools);
    let mut results = Vec::new();

    for batch in batches {
        if batch.is_concurrent && batch.calls.len() > 1 {
            // Run concurrency-safe tools in parallel, capped at MAX_CONCURRENT
            let mut chunk_results = Vec::new();
            for chunk in batch.calls.chunks(MAX_CONCURRENT) {
                let futures: Vec<_> = chunk.iter().cloned().map(&executor).collect();
                chunk_results.extend(join_all(futures).await);
            }
            results.extend(chunk_results);
        } else {
            // Run serially (either unsafe or a single safe tool)
            for tc in batch.calls {
                results.push(executor(tc).await);
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Tool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn make_tool(name: &str, concurrent: bool) -> Tool {
        Tool {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
            is_concurrency_safe: concurrent,
        }
    }

    fn make_call(name: &str, id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn test_partition_groups_consecutive_safe_tools() {
        let tools = vec![
            make_tool("read", true),
            make_tool("grep", true),
            make_tool("write", false),
        ];
        let calls = vec![
            make_call("read", "1"),
            make_call("grep", "2"),
            make_call("write", "3"),
        ];

        let batches = partition_tool_calls(calls, &tools);
        assert_eq!(batches.len(), 2);
        assert!(batches[0].is_concurrent);
        assert_eq!(batches[0].calls.len(), 2);
        assert!(!batches[1].is_concurrent);
        assert_eq!(batches[1].calls.len(), 1);
    }

    #[test]
    fn test_partition_unsafe_breaks_groups() {
        let tools = vec![
            make_tool("read", true),
            make_tool("write", false),
            make_tool("grep", true),
        ];
        let calls = vec![
            make_call("read", "1"),
            make_call("write", "2"),
            make_call("grep", "3"),
        ];

        let batches = partition_tool_calls(calls, &tools);
        assert_eq!(batches.len(), 3);
        assert!(batches[0].is_concurrent);
        assert!(!batches[1].is_concurrent);
        assert!(batches[2].is_concurrent);
    }

    #[test]
    fn test_partition_unknown_tools_are_unsafe() {
        let tools = vec![make_tool("read", true)];
        let calls = vec![
            make_call("read", "1"),
            make_call("unknown", "2"),
        ];

        let batches = partition_tool_calls(calls, &tools);
        assert_eq!(batches.len(), 2);
        assert!(batches[0].is_concurrent);
        assert!(!batches[1].is_concurrent);
    }

    #[test]
    fn test_partition_all_safe() {
        let tools = vec![
            make_tool("read", true),
            make_tool("grep", true),
            make_tool("glob", true),
        ];
        let calls = vec![
            make_call("read", "1"),
            make_call("grep", "2"),
            make_call("glob", "3"),
        ];

        let batches = partition_tool_calls(calls, &tools);
        assert_eq!(batches.len(), 1);
        assert!(batches[0].is_concurrent);
        assert_eq!(batches[0].calls.len(), 3);
    }

    #[test]
    fn test_partition_all_unsafe() {
        let tools = vec![
            make_tool("write", false),
            make_tool("bash", false),
        ];
        let calls = vec![
            make_call("write", "1"),
            make_call("bash", "2"),
        ];

        let batches = partition_tool_calls(calls, &tools);
        assert_eq!(batches.len(), 2);
        assert!(!batches[0].is_concurrent);
        assert!(!batches[1].is_concurrent);
    }

    #[test]
    fn test_partition_empty() {
        let batches = partition_tool_calls(vec![], &[]);
        assert!(batches.is_empty());
    }

    #[tokio::test]
    async fn test_execute_safe_tools_run_concurrently() {
        let tools = vec![
            make_tool("read", true),
            make_tool("grep", true),
        ];
        let calls = vec![
            make_call("read", "1"),
            make_call("grep", "2"),
        ];

        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));

        let max_c = Arc::clone(&max_concurrent);
        let cur = Arc::clone(&current);

        let results = execute_tool_calls(calls, &tools, move |tc| {
            let max_c = Arc::clone(&max_c);
            let cur = Arc::clone(&cur);
            async move {
                let c = cur.fetch_add(1, Ordering::SeqCst) + 1;
                max_c.fetch_max(c, Ordering::SeqCst);
                tokio::task::yield_now().await;
                cur.fetch_sub(1, Ordering::SeqCst);
                ToolResult::success(tc.id, format!("result_{}", tc.name))
            }
        })
        .await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_call_id, "1");
        assert_eq!(results[1].tool_call_id, "2");
        // Both should have been in-flight at the same time
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_execute_unsafe_tools_run_serially() {
        let tools = vec![
            make_tool("write", false),
            make_tool("bash", false),
        ];
        let calls = vec![
            make_call("write", "1"),
            make_call("bash", "2"),
        ];

        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));

        let max_c = Arc::clone(&max_concurrent);
        let cur = Arc::clone(&current);

        let results = execute_tool_calls(calls, &tools, move |tc| {
            let max_c = Arc::clone(&max_c);
            let cur = Arc::clone(&cur);
            async move {
                let c = cur.fetch_add(1, Ordering::SeqCst) + 1;
                max_c.fetch_max(c, Ordering::SeqCst);
                tokio::task::yield_now().await;
                cur.fetch_sub(1, Ordering::SeqCst);
                ToolResult::success(tc.id, format!("result_{}", tc.name))
            }
        })
        .await;

        assert_eq!(results.len(), 2);
        // Serial execution: max concurrency should be 1
        assert_eq!(max_concurrent.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_execute_mixed_preserves_order() {
        let tools = vec![
            make_tool("read", true),
            make_tool("grep", true),
            make_tool("write", false),
            make_tool("glob", true),
        ];
        let calls = vec![
            make_call("read", "1"),
            make_call("grep", "2"),
            make_call("write", "3"),
            make_call("glob", "4"),
        ];

        let results = execute_tool_calls(calls, &tools, |tc| async move {
            ToolResult::success(tc.id, format!("done_{}", tc.name))
        })
        .await;

        assert_eq!(results.len(), 4);
        assert_eq!(results[0].tool_call_id, "1");
        assert_eq!(results[1].tool_call_id, "2");
        assert_eq!(results[2].tool_call_id, "3");
        assert_eq!(results[3].tool_call_id, "4");
    }
}
