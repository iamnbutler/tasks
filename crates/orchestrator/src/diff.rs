//! Unified diff parser for extracting file paths and changed line numbers.
//!
//! Used by the orchestrator to proactively fetch context around changed lines
//! during PR evaluation, reducing the need for pass-2 deep reviews.

/// A file that appears in a unified diff, with its changed line numbers.
#[derive(Debug, Clone)]
pub struct DiffFile {
    /// Path of the changed file (e.g., "src/main.rs").
    pub path: String,
    /// Line numbers that were changed in the new version of the file.
    /// These are line numbers from the `+` side of the diff (additions/modifications).
    pub changed_lines: Vec<usize>,
}

/// Parse a unified diff to extract file paths and changed line numbers.
///
/// Returns files sorted by number of changed lines (most changed first).
pub fn parse_diff_files(diff: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_lines: Vec<usize> = Vec::new();
    // Current line number in the new file (right side of hunk header)
    let mut new_line: usize = 0;

    for line in diff.lines() {
        if line.starts_with("+++ b/") {
            // Flush previous file
            if let Some(path) = current_path.take() {
                if !current_lines.is_empty() {
                    files.push(DiffFile {
                        path,
                        changed_lines: std::mem::take(&mut current_lines),
                    });
                }
            }
            current_path = Some(line[6..].to_string());
            current_lines.clear();
        } else if line.starts_with("+++ /dev/null") {
            // File was deleted — skip it
            if let Some(path) = current_path.take() {
                if !current_lines.is_empty() {
                    files.push(DiffFile {
                        path,
                        changed_lines: std::mem::take(&mut current_lines),
                    });
                }
            }
            current_path = None;
            current_lines.clear();
        } else if line.starts_with("@@ ") {
            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            if let Some(new_start) = parse_hunk_header_new_start(line) {
                new_line = new_start;
            }
        } else if current_path.is_some() {
            if let Some(stripped) = line.strip_prefix('+') {
                // Added/modified line — not the +++ header
                if !stripped.starts_with("++ ") {
                    current_lines.push(new_line);
                    new_line += 1;
                }
            } else if line.starts_with('-') {
                // Removed line — doesn't advance new_line counter
            } else {
                // Context line — advances new_line counter
                new_line += 1;
            }
        }
    }

    // Flush last file
    if let Some(path) = current_path {
        if !current_lines.is_empty() {
            files.push(DiffFile {
                path,
                changed_lines: current_lines,
            });
        }
    }

    // Sort by most changed lines first
    files.sort_by(|a, b| b.changed_lines.len().cmp(&a.changed_lines.len()));
    files
}

/// Parse the new-file start line from a hunk header.
///
/// Format: `@@ -old_start[,old_count] +new_start[,new_count] @@`
fn parse_hunk_header_new_start(line: &str) -> Option<usize> {
    let plus_idx = line.find('+').filter(|&i| i > 0)?;
    let rest = &line[plus_idx + 1..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Extract context windows around changed lines from file content.
///
/// Returns lines with line numbers, showing ±`window` lines around each change.
/// Adjacent windows are merged. Total output is capped at `max_lines`.
pub fn extract_context_window(
    content: &str,
    changed_lines: &[usize],
    window: usize,
    max_lines: usize,
) -> String {
    if changed_lines.is_empty() {
        return String::new();
    }

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    // Build a set of line numbers to include (1-indexed)
    let mut include = vec![false; total_lines + 1];
    for &line_num in changed_lines {
        let start = line_num.saturating_sub(window).max(1);
        let end = (line_num + window).min(total_lines);
        for i in start..=end {
            include[i] = true;
        }
    }

    let mut result = Vec::new();
    let mut in_window = false;

    for (idx, line_text) in lines.iter().enumerate() {
        let line_num = idx + 1;
        if line_num >= include.len() {
            break;
        }
        if include[line_num] {
            if !in_window && !result.is_empty() {
                result.push("...".to_string());
            }
            in_window = true;
            result.push(format!("{}: {}", line_num, line_text));
            if result.len() >= max_lines {
                result.push("... (truncated)".to_string());
                break;
            }
        } else {
            in_window = false;
        }
    }

    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_diff_files_basic() {
        let diff = r#"diff --git a/src/main.rs b/src/main.rs
index abc123..def456 100644
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
    }

    #[test]
    fn test_parse_diff_files_multiple() {
        let diff = r#"diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,4 @@
 line1
+added1
+added2
 line2
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -5,3 +5,4 @@
 line5
+added3
 line6
"#;
        let files = parse_diff_files(diff);
        assert_eq!(files.len(), 2);
        // Most changed first
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].changed_lines.len(), 2);
        assert_eq!(files[1].path, "src/b.rs");
        assert_eq!(files[1].changed_lines.len(), 1);
    }

    #[test]
    fn test_parse_diff_files_deleted_file() {
        let diff = r#"diff --git a/src/old.rs b/src/old.rs
--- a/src/old.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-line1
-line2
-line3
"#;
        let files = parse_diff_files(diff);
        // Deleted files have no added lines
        assert!(files.is_empty());
    }

    #[test]
    fn test_parse_hunk_header() {
        assert_eq!(parse_hunk_header_new_start("@@ -10,6 +10,7 @@ fn main()"), Some(10));
        assert_eq!(parse_hunk_header_new_start("@@ -1 +1,4 @@"), Some(1));
        assert_eq!(parse_hunk_header_new_start("@@ -0,0 +1,20 @@"), Some(1));
    }

    #[test]
    fn test_extract_context_window() {
        let content = (1..=20)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");

        let result = extract_context_window(&content, &[10], 2, 100);
        assert!(result.contains("8: line 8"));
        assert!(result.contains("10: line 10"));
        assert!(result.contains("12: line 12"));
        assert!(!result.contains("7: line 7"));
        assert!(!result.contains("13: line 13"));
    }

    #[test]
    fn test_extract_context_window_merged_ranges() {
        let content = (1..=20)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");

        // Two changes close together — windows should merge
        let result = extract_context_window(&content, &[5, 7], 2, 100);
        assert!(result.contains("3: line 3"));
        assert!(result.contains("9: line 9"));
        // No ellipsis between them since windows overlap
        assert!(!result.contains("..."));
    }

    #[test]
    fn test_extract_context_window_max_lines() {
        let content = (1..=100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");

        let result = extract_context_window(&content, &[50], 50, 10);
        let line_count = result.lines().count();
        // Should be capped at max_lines + truncation message
        assert!(line_count <= 11);
        assert!(result.contains("truncated"));
    }
}
