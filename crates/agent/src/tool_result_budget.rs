//! Tool result budgeting: persist large outputs to disk and return a preview.
//!
//! Prevents context bloat from large tool outputs (e.g., verbose test output,
//! large file contents). When a tool result exceeds the configured max size,
//! the full output is written to disk and a preview + file path is returned
//! to the model.

use std::path::{Path, PathBuf};

use crate::message::{Tool, ToolCall, ToolResult, DEFAULT_MAX_RESULT_SIZE};

/// Size of the preview returned to the model when output is persisted.
const PREVIEW_SIZE: usize = 2_000;

/// Budget a single tool result, persisting to disk if it exceeds the max size.
///
/// Returns the (possibly truncated) content string. If the result is persisted,
/// the returned string contains the file path and a preview.
pub fn budget_tool_result(
    content: &str,
    tool_name: &str,
    max_size: Option<usize>,
    output_dir: &Path,
) -> String {
    let max = match max_size {
        Some(max) => max,
        None => return content.to_string(), // No budgeting for this tool
    };

    if content.len() <= max {
        return content.to_string();
    }

    // Persist full output to disk
    let filename = format!(
        "tool_output_{}_{}.txt",
        tool_name,
        uuid::Uuid::new_v4()
    );
    let path = output_dir.join(&filename);

    if let Err(e) = std::fs::create_dir_all(output_dir) {
        tracing::warn!(
            tool_name,
            path = %output_dir.display(),
            error = %e,
            "failed to create output directory for tool result; returning full result"
        );
        return content.to_string();
    }

    if let Err(e) = std::fs::write(&path, content) {
        tracing::warn!(
            tool_name,
            path = %path.display(),
            error = %e,
            "failed to persist tool result to disk; returning full result"
        );
        return content.to_string();
    }

    let preview_end = find_char_boundary(content, PREVIEW_SIZE);
    let preview = &content[..preview_end];

    tracing::info!(
        tool_name,
        original_size = content.len(),
        path = %path.display(),
        "persisted large tool result to disk"
    );

    format!(
        "Output ({} bytes) saved to: {}\n\nPreview:\n{}",
        content.len(),
        path.display(),
        preview,
    )
}

/// Apply tool result budgeting to a batch of tool results.
///
/// Looks up each result's tool name from the pending tool calls, finds the
/// corresponding tool definition, and applies budgeting based on `max_result_size`.
pub fn budget_tool_results(
    results: Vec<ToolResult>,
    pending_tool_calls: &[ToolCall],
    tools: &[Tool],
    output_dir: &Path,
) -> Vec<ToolResult> {
    results
        .into_iter()
        .map(|result| {
            let max_size = resolve_max_result_size(
                &result.tool_call_id,
                pending_tool_calls,
                tools,
            );
            let budgeted_content = budget_tool_result(
                &result.content,
                &tool_name_for_call(&result.tool_call_id, pending_tool_calls)
                    .unwrap_or("unknown"),
                max_size,
                output_dir,
            );
            ToolResult {
                tool_call_id: result.tool_call_id,
                content: budgeted_content,
                is_error: result.is_error,
            }
        })
        .collect()
}

/// Resolve the max_result_size for a tool call by looking up the tool definition.
fn resolve_max_result_size(
    tool_call_id: &str,
    pending_tool_calls: &[ToolCall],
    tools: &[Tool],
) -> Option<usize> {
    let tool_name = tool_name_for_call(tool_call_id, pending_tool_calls)?;
    let tool_def = tools.iter().find(|t| t.name == tool_name);
    match tool_def {
        Some(t) => t.max_result_size,
        None => Some(DEFAULT_MAX_RESULT_SIZE),
    }
}

/// Look up the tool name for a given tool call ID.
fn tool_name_for_call<'a>(
    tool_call_id: &str,
    pending_tool_calls: &'a [ToolCall],
) -> Option<&'a str> {
    pending_tool_calls
        .iter()
        .find(|tc| tc.id == tool_call_id)
        .map(|tc| tc.name.as_str())
}

/// Find a char boundary at or before `index` to avoid splitting a multi-byte character.
fn find_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    // Use the floor_char_boundary approach
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Return the path where tool output files are stored for a given session.
pub fn tool_output_dir(base_dir: &Path, session_id: &str) -> PathBuf {
    base_dir.join("sessions").join(session_id).join("tool_outputs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Tool;
    use std::fs;
    use tempfile::TempDir;

    fn make_tool(name: &str, max_result_size: Option<usize>) -> Tool {
        Tool {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
            is_concurrency_safe: false,
            max_result_size,
        }
    }

    fn make_tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn test_small_result_passes_through() {
        let dir = TempDir::new().unwrap();
        let result = budget_tool_result("hello", "test", Some(100), dir.path());
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_large_result_is_persisted() {
        let dir = TempDir::new().unwrap();
        let big_output = "x".repeat(200);
        let result = budget_tool_result(&big_output, "bash", Some(100), dir.path());

        assert!(result.contains("Output (200 bytes) saved to:"));
        assert!(result.contains("Preview:"));

        // Verify the file was written
        let files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1);
        let saved = fs::read_to_string(files[0].path()).unwrap();
        assert_eq!(saved, big_output);
    }

    #[test]
    fn test_none_max_size_never_persists() {
        let dir = TempDir::new().unwrap();
        let big_output = "x".repeat(1_000_000);
        let result = budget_tool_result(&big_output, "read", None, dir.path());
        assert_eq!(result, big_output);
    }

    #[test]
    fn test_preview_size_limit() {
        let dir = TempDir::new().unwrap();
        let big_output = "a".repeat(10_000);
        let result = budget_tool_result(&big_output, "bash", Some(100), dir.path());

        // Preview should be at most PREVIEW_SIZE bytes
        let preview_marker = "Preview:\n";
        let preview_start = result.find(preview_marker).unwrap() + preview_marker.len();
        let preview = &result[preview_start..];
        assert!(preview.len() <= PREVIEW_SIZE);
    }

    #[test]
    fn test_budget_tool_results_batch() {
        let dir = TempDir::new().unwrap();
        let tools = vec![
            make_tool("bash", Some(50)),
            make_tool("read", None),
        ];
        let pending = vec![
            make_tool_call("tc-1", "bash"),
            make_tool_call("tc-2", "read"),
        ];
        let results = vec![
            ToolResult::success("tc-1", "x".repeat(100)),
            ToolResult::success("tc-2", "x".repeat(100)),
        ];

        let budgeted = budget_tool_results(results, &pending, &tools, dir.path());

        // bash result should be persisted (100 > 50)
        assert!(budgeted[0].content.contains("Output (100 bytes) saved to:"));
        // read result should pass through (max_result_size = None)
        assert_eq!(budgeted[1].content.len(), 100);
    }

    #[test]
    fn test_unknown_tool_uses_default_limit() {
        let max = resolve_max_result_size(
            "tc-1",
            &[make_tool_call("tc-1", "unknown_tool")],
            &[], // no tool definitions
        );
        assert_eq!(max, Some(DEFAULT_MAX_RESULT_SIZE));
    }

    #[test]
    fn test_multibyte_preview_boundary() {
        let dir = TempDir::new().unwrap();
        // Create a string with multi-byte characters near the preview boundary
        let mut s = "a".repeat(PREVIEW_SIZE - 2);
        s.push('é'); // 2-byte char
        s.push_str(&"b".repeat(DEFAULT_MAX_RESULT_SIZE)); // make it large enough
        let result = budget_tool_result(&s, "bash", Some(100), dir.path());
        assert!(result.contains("Preview:"));
        // The preview should be valid UTF-8
        let preview_marker = "Preview:\n";
        let preview_start = result.find(preview_marker).unwrap() + preview_marker.len();
        let _preview = &result[preview_start..]; // would panic if invalid UTF-8
    }
}
