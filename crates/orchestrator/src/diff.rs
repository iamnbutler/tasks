//! Unified diff parser for extracting file paths and changed line numbers.
//!
//! Used to proactively fetch context around changed code during review.

use std::collections::HashSet;

/// A file that was changed in a diff, with the line numbers that were modified.
#[derive(Debug, Clone)]
pub struct DiffFile {
    /// Path of the changed file (e.g., "src/main.rs").
    pub path: String,
    /// Line numbers that were added or modified in the new version.
    pub changed_lines: Vec<usize>,
    /// Total number of changed lines (additions + deletions).
    pub change_count: usize,
}

/// Parse a unified diff to extract changed files and their modified line numbers.
///
/// Returns files sorted by change count (most changed first).
pub fn parse_diff_files(diff: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_lines: Vec<usize> = Vec::new();
    let mut current_change_count: usize = 0;
    let mut new_line_num: usize = 0;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            // Flush previous file
            if let Some(path) = current_path.take() {
                files.push(DiffFile {
                    path,
                    changed_lines: current_lines.clone(),
                    change_count: current_change_count,
                });
            }
            current_lines.clear();
            current_change_count = 0;
            new_line_num = 0;

            // Extract path from "diff --git a/path b/path"
            if let Some(b_path) = line.split(" b/").last() {
                current_path = Some(b_path.to_string());
            }
        } else if line.starts_with("+++ b/") {
            // More reliable path from the +++ line
            if let Some(path) = line.strip_prefix("+++ b/") {
                current_path = Some(path.to_string());
            }
        } else if line.starts_with("@@ ") {
            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            if let Some(new_range) = parse_hunk_header(line) {
                new_line_num = new_range;
            }
        } else if new_line_num > 0 {
            if line.starts_with('+') && !line.starts_with("+++") {
                current_lines.push(new_line_num);
                current_change_count += 1;
                new_line_num += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                current_change_count += 1;
                // Deletion — don't advance new_line_num
            } else {
                // Context line
                new_line_num += 1;
            }
        }
    }

    // Flush last file
    if let Some(path) = current_path {
        files.push(DiffFile {
            path,
            changed_lines: current_lines,
            change_count: current_change_count,
        });
    }

    // Sort by change count descending
    files.sort_by(|a, b| b.change_count.cmp(&a.change_count));
    files
}

/// Parse a hunk header to get the new file start line number.
///
/// Format: `@@ -old_start[,old_count] +new_start[,new_count] @@`
fn parse_hunk_header(line: &str) -> Option<usize> {
    let plus_idx = line.find('+')?;
    let rest = &line[plus_idx + 1..];
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

/// Extract context windows around changed lines from file content.
///
/// Returns line-numbered content with ±`context_lines` around each change,
/// collapsing overlapping windows. Limits total output to `max_lines`.
pub fn extract_context_window(
    content: &str,
    changed_lines: &[usize],
    context_lines: usize,
    max_lines: usize,
) -> String {
    if changed_lines.is_empty() {
        // No specific lines — return the head of the file
        return content
            .lines()
            .take(max_lines)
            .enumerate()
            .map(|(i, line)| format!("{}: {}", i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
    }

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    // Build set of lines to include
    let target_lines: HashSet<usize> = changed_lines
        .iter()
        .flat_map(|&line| {
            let start = line.saturating_sub(context_lines);
            let end = (line + context_lines).min(total_lines);
            (start.max(1))..=end
        })
        .collect();

    let mut sorted_lines: Vec<usize> = target_lines.into_iter().collect();
    sorted_lines.sort_unstable();

    let mut result = Vec::new();
    let mut prev_line: Option<usize> = None;

    for &line_num in sorted_lines.iter().take(max_lines) {
        if line_num == 0 || line_num > total_lines {
            continue;
        }
        // Insert separator for gaps
        if let Some(prev) = prev_line {
            if line_num > prev + 1 {
                result.push("...".to_string());
            }
        }
        result.push(format!("{}: {}", line_num, lines[line_num - 1]));
        prev_line = Some(line_num);
    }

    result.join("\n")
}

/// Maximum number of files to proactively fetch context for.
pub const MAX_CONTEXT_FILES: usize = 3;

/// Number of context lines to include around each change.
pub const CONTEXT_WINDOW_LINES: usize = 50;

/// Maximum lines of context per file.
pub const MAX_CONTEXT_LINES_PER_FILE: usize = 300;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_diff_files_basic() {
        let diff = r#"diff --git a/src/main.rs b/src/main.rs
index 1234567..abcdefg 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,6 +10,7 @@ fn main() {
     let x = 1;
     let y = 2;
+    let z = 3;
     println!("hello");
 }
"#;
        let files = parse_diff_files(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/main.rs");
        assert_eq!(files[0].changed_lines, vec![12]);
        assert_eq!(files[0].change_count, 1);
    }

    #[test]
    fn test_parse_diff_files_multiple() {
        let diff = r#"diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 line1
+added
 line2
 line3
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -1,5 +1,7 @@
 line1
+added1
+added2
 line2
-removed
+replaced
 line4
"#;
        let files = parse_diff_files(diff);
        assert_eq!(files.len(), 2);
        // b.rs has more changes (3 additions + 1 deletion = 4) vs a.rs (1)
        assert_eq!(files[0].path, "src/b.rs");
        assert_eq!(files[0].change_count, 4);
        assert_eq!(files[1].path, "src/a.rs");
        assert_eq!(files[1].change_count, 1);
    }

    #[test]
    fn test_parse_diff_files_empty() {
        assert!(parse_diff_files("").is_empty());
        assert!(parse_diff_files("no diff here").is_empty());
    }

    #[test]
    fn test_parse_hunk_header() {
        assert_eq!(parse_hunk_header("@@ -10,6 +10,7 @@ fn main() {"), Some(10));
        assert_eq!(parse_hunk_header("@@ -1,3 +1,4 @@"), Some(1));
        assert_eq!(parse_hunk_header("@@ -0,0 +1,5 @@"), Some(1));
    }

    #[test]
    fn test_extract_context_window_basic() {
        let content = (1..=20)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");

        let result = extract_context_window(&content, &[10], 3, 100);
        assert!(result.contains("7: line 7"));
        assert!(result.contains("10: line 10"));
        assert!(result.contains("13: line 13"));
        // Should not contain lines far from the change
        assert!(!result.starts_with("1: "));
        assert!(!result.contains("\n1: "));
        assert!(!result.contains("20: line 20"));
    }

    #[test]
    fn test_extract_context_window_gap_separator() {
        let content = (1..=100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");

        // Two distant changes should produce a gap
        let result = extract_context_window(&content, &[10, 90], 2, 200);
        assert!(result.contains("..."));
    }

    #[test]
    fn test_extract_context_window_empty_lines() {
        let content = "line1\nline2\nline3";
        let result = extract_context_window(&content, &[], 3, 100);
        assert!(result.contains("1: line1"));
    }
}
