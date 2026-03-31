//! Unified diff parser for extracting file paths and changed line numbers.
//!
//! Used by the orchestrator to proactively fetch context around changed lines
//! during PR review, so the reviewer sees surrounding code — not just isolated hunks.

use std::collections::HashSet;

/// A file that was changed in a diff, with the line numbers that were modified.
#[derive(Debug, Clone)]
pub struct DiffFile {
    /// Path of the changed file (e.g. "src/auth.rs").
    pub path: String,
    /// Line numbers changed in the new (post-image) version of the file.
    pub changed_lines: Vec<usize>,
    /// Total number of added + removed lines (used for ranking "most changed").
    pub change_count: usize,
}

/// Parse a unified diff to extract changed files and their changed line numbers.
///
/// Handles standard `diff --git a/path b/path` and `--- a/path` / `+++ b/path` headers.
/// Extracts new-side line numbers from `@@ -old,count +new,count @@` hunk headers.
pub fn parse_diff_files(diff: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut changed_lines: Vec<usize> = Vec::new();
    let mut change_count: usize = 0;
    let mut new_line_num: usize = 0;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            // Flush previous file
            if let Some(path) = current_path.take() {
                if !changed_lines.is_empty() {
                    files.push(DiffFile {
                        path,
                        changed_lines: std::mem::take(&mut changed_lines),
                        change_count,
                    });
                }
            }
            change_count = 0;
            new_line_num = 0;
        } else if line.starts_with("+++ b/") {
            current_path = Some(line[6..].to_string());
        } else if line.starts_with("+++ /dev/null") {
            // File was deleted — no new-side content to fetch
            current_path = None;
        } else if line.starts_with("@@ ") {
            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            if let Some(new_start) = parse_hunk_new_start(line) {
                new_line_num = new_start;
            }
        } else if current_path.is_some() && new_line_num > 0 {
            if line.starts_with('+') && !line.starts_with("+++") {
                // Added line
                changed_lines.push(new_line_num);
                change_count += 1;
                new_line_num += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                // Removed line — doesn't advance new line counter
                change_count += 1;
            } else if !line.starts_with('\\') {
                // Context line (or empty)
                new_line_num += 1;
            }
        }
    }

    // Flush last file
    if let Some(path) = current_path {
        if !changed_lines.is_empty() {
            files.push(DiffFile {
                path,
                changed_lines,
                change_count,
            });
        }
    }

    // Sort by change_count descending (most-changed first)
    files.sort_by(|a, b| b.change_count.cmp(&a.change_count));
    files
}

/// Parse the new-side start line from a hunk header.
///
/// Input: `@@ -10,5 +20,8 @@ fn example()`
/// Returns: `Some(20)`
fn parse_hunk_new_start(line: &str) -> Option<usize> {
    let after_plus = line.find('+').map(|i| &line[i + 1..])?;
    let end = after_plus.find(|c: char| c == ',' || c == ' ')?;
    after_plus[..end].parse().ok()
}

/// Extract a context window around changed lines from file content.
///
/// Returns numbered lines (1-indexed) within ±`window` lines of any changed line.
/// Discontinuous ranges are separated by `...` markers.
pub fn extract_context_window(content: &str, changed_lines: &[usize], window: usize) -> String {
    if changed_lines.is_empty() {
        return String::new();
    }

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    // Build set of lines to include
    let include: HashSet<usize> = changed_lines
        .iter()
        .flat_map(|&line| {
            let start = line.saturating_sub(window).max(1);
            let end = (line + window).min(total);
            start..=end
        })
        .collect();

    let mut result = String::new();
    let mut prev_included = false;

    for (i, line) in lines.iter().enumerate() {
        let line_num = i + 1; // 1-indexed
        if include.contains(&line_num) {
            if !prev_included && !result.is_empty() {
                result.push_str("...\n");
            }
            result.push_str(&format!("{}: {}\n", line_num, line));
            prev_included = true;
        } else {
            prev_included = false;
        }
    }

    result
}

/// Maximum number of files to proactively fetch context for.
pub const MAX_CONTEXT_FILES: usize = 3;

/// Maximum total lines of context to include (across all files).
pub const MAX_CONTEXT_LINES: usize = 1000;

/// Default context window size (lines before/after each change).
pub const CONTEXT_WINDOW: usize = 50;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_diff_files_basic() {
        let diff = "\
diff --git a/src/auth.rs b/src/auth.rs
index abc..def 100644
--- a/src/auth.rs
+++ b/src/auth.rs
@@ -10,3 +10,4 @@ fn login() {
     let user = get_user();
+    validate(user);
     Ok(user)
 }
";
        let files = parse_diff_files(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/auth.rs");
        assert_eq!(files[0].changed_lines, vec![11]);
        assert_eq!(files[0].change_count, 1);
    }

    #[test]
    fn test_parse_diff_files_multiple() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 line1
+added_a1
+added_a2
 line2
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -5,2 +5,3 @@
 old
+added_b
 more
";
        let files = parse_diff_files(diff);
        assert_eq!(files.len(), 2);
        // Sorted by change_count desc: a has 2, b has 1
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].change_count, 2);
        assert_eq!(files[1].path, "src/b.rs");
        assert_eq!(files[1].change_count, 1);
    }

    #[test]
    fn test_parse_diff_files_deleted_file() {
        let diff = "\
diff --git a/src/old.rs b/src/old.rs
--- a/src/old.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-line1
-line2
-line3
";
        let files = parse_diff_files(diff);
        assert!(files.is_empty(), "Deleted files should not be included");
    }

    #[test]
    fn test_parse_diff_files_removals_and_additions() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,4 +10,4 @@ fn example() {
-    old_call();
+    new_call();
     keep();
 }
";
        let files = parse_diff_files(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].changed_lines, vec![10]);
        assert_eq!(files[0].change_count, 2); // 1 removal + 1 addition
    }

    #[test]
    fn test_parse_hunk_new_start() {
        assert_eq!(parse_hunk_new_start("@@ -10,5 +20,8 @@ fn example()"), Some(20));
        assert_eq!(parse_hunk_new_start("@@ -1 +1 @@"), Some(1));
        assert_eq!(parse_hunk_new_start("@@ -0,0 +1,5 @@"), Some(1));
    }

    #[test]
    fn test_extract_context_window_basic() {
        let content = (1..=20)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");

        let result = extract_context_window(&content, &[10], 3);
        // Should include lines 7-13
        assert!(result.contains("7: line 7"));
        assert!(result.contains("10: line 10"));
        assert!(result.contains("13: line 13"));
        assert!(!result.contains("6: line 6"));
        assert!(!result.contains("14: line 14"));
    }

    #[test]
    fn test_extract_context_window_discontinuous() {
        let content = (1..=100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");

        let result = extract_context_window(&content, &[10, 90], 3);
        assert!(result.contains("..."));
        assert!(result.contains("10: line 10"));
        assert!(result.contains("90: line 90"));
    }

    #[test]
    fn test_extract_context_window_empty() {
        assert_eq!(extract_context_window("some content", &[], 50), "");
    }

    #[test]
    fn test_extract_context_window_clamps_to_bounds() {
        let content = "line1\nline2\nline3";
        let result = extract_context_window(content, &[1], 100);
        assert!(result.contains("1: line1"));
        assert!(result.contains("3: line3"));
    }
}
