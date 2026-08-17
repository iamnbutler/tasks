//! The Agent Feed's row model: raw Claude Code stream-json transcript lines
//! folded into renderable rows.
//!
//! Deliberately gpui-free and tested, like [`crate::chat_log`]. The server
//! stores the agent's stdout verbatim (`TranscriptLine`), so the shape of a
//! line belongs to Claude Code and can change under us — every parse here is
//! defensive: a record we don't recognize is skipped quietly, a line that is
//! not JSON at all renders as raw text, and the two capture contracts from
//! `crates/tasks/src/transcript.rs` are honored as notices:
//!
//! - a line prefixed `"[tasks: truncated N bytes]"` was cut at 32 KiB and
//!   renders as a truncation notice, never as a wall of escaped JSON;
//! - a line prefixed `"[tasks] "` is the capture layer speaking (dropped
//!   lines, the per-run cap), and renders as a seam, not as agent output.
//!
//! Consecutive tool calls coalesce into one row, the chat's idiom: a dozen
//! calls are one step of the agent's work, not a dozen rows.

use tasks_client::api::models::{TranscriptLine, TranscriptStream};

/// Marker prefix for a line the capture layer truncated.
const TRUNCATED_PREFIX: &str = "[tasks: truncated ";
/// Marker prefix for the capture layer's own notices.
const CAPTURE_PREFIX: &str = "[tasks] ";

/// One renderable row of the feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedRow {
    /// The seq of the first transcript line in this row — the stable key a
    /// virtualized list diffs on. Tool rows keep their first call's seq as
    /// later calls coalesce in, so a growing group is the *same* row.
    pub seq: i64,
    pub kind: FeedRowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FeedRowKind {
    /// An assistant text record — markdown.
    Text(String),
    /// One or more consecutive tool calls, by label.
    Tools(Vec<String>),
    /// A quiet one-liner: session start, compaction, the run's conclusion,
    /// or a capture-layer seam.
    Notice(String),
    /// A stdout line that wasn't JSON, or a stderr line: shown as-is.
    Raw(String),
}

/// The list key a row diffs by: (seq, kind discriminant). The discriminant
/// matters because a row's *content* may grow (a tool group) without the row
/// changing identity, while a different kind at the same seq is a different
/// row.
pub(crate) type FeedKey = (i64, u8);

impl FeedRow {
    pub fn key(&self) -> FeedKey {
        let kind = match self.kind {
            FeedRowKind::Text(_) => 0,
            FeedRowKind::Tools(_) => 1,
            FeedRowKind::Notice(_) => 2,
            FeedRowKind::Raw(_) => 3,
        };
        (self.seq, kind)
    }
}

/// What one transcript line contributes to the feed.
enum Parsed {
    Text(String),
    Tool(String),
    Notice(String),
    Raw(String),
    /// A record that renders as nothing — tool results (they fold into the
    /// call), unknown record kinds, empty text.
    Nothing,
}

/// Fold a run's transcript into rows.
pub(crate) fn feed_rows(lines: &[TranscriptLine]) -> Vec<FeedRow> {
    let mut rows: Vec<FeedRow> = Vec::new();
    for line in lines {
        match parse_line(line) {
            Parsed::Nothing => {}
            Parsed::Tool(label) => match rows.last_mut() {
                // Coalesce into an open group. Anything else on screen in
                // between (a notice, raw stderr) breaks the group, so the
                // feed never reorders what actually happened.
                Some(FeedRow {
                    kind: FeedRowKind::Tools(labels),
                    ..
                }) => labels.push(label),
                _ => rows.push(FeedRow {
                    seq: line.seq,
                    kind: FeedRowKind::Tools(vec![label]),
                }),
            },
            Parsed::Text(text) => rows.push(FeedRow {
                seq: line.seq,
                kind: FeedRowKind::Text(text),
            }),
            Parsed::Notice(text) => rows.push(FeedRow {
                seq: line.seq,
                kind: FeedRowKind::Notice(text),
            }),
            Parsed::Raw(text) => rows.push(FeedRow {
                seq: line.seq,
                kind: FeedRowKind::Raw(text),
            }),
        }
    }
    rows
}

fn parse_line(line: &TranscriptLine) -> Parsed {
    // The capture layer's contracts come before any parsing: a truncated
    // record is not valid JSON, and a capture notice never was.
    if line.line.starts_with(TRUNCATED_PREFIX) {
        return Parsed::Notice("a record was truncated — too large to keep whole".to_string());
    }
    if let Some(rest) = line.line.strip_prefix(CAPTURE_PREFIX) {
        return Parsed::Notice(rest.to_string());
    }
    if line.stream == TranscriptStream::Stderr {
        let text = line.line.trim();
        if text.is_empty() {
            return Parsed::Nothing;
        }
        return Parsed::Raw(line.line.clone());
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(&line.line) else {
        let text = line.line.trim();
        if text.is_empty() {
            return Parsed::Nothing;
        }
        return Parsed::Raw(line.line.clone());
    };

    match value.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => {
            // One assistant record can carry text and tool calls together;
            // text wins the row and the calls follow — but in practice
            // Claude Code emits them as separate records. Handle both.
            let Some(content) = value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            else {
                return Parsed::Nothing;
            };
            let mut text = String::new();
            let mut tool: Option<String> = None;
            for item in content {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(t);
                        }
                    }
                    Some("tool_use") if tool.is_none() => tool = Some(tool_label(item)),
                    _ => {}
                }
            }
            if !text.trim().is_empty() {
                Parsed::Text(text)
            } else if let Some(label) = tool {
                Parsed::Tool(label)
            } else {
                Parsed::Nothing
            }
        }
        // Tool results fold into the call that made them.
        Some("user") => Parsed::Nothing,
        Some("system") => match value.get("subtype").and_then(|s| s.as_str()) {
            Some("init") => {
                let model = value
                    .get("model")
                    .and_then(|m| m.as_str())
                    .unwrap_or("agent");
                Parsed::Notice(format!("session started · {model}"))
            }
            Some("compact_boundary") | Some("compaction") => {
                Parsed::Notice("context compacted".to_string())
            }
            _ => Parsed::Nothing,
        },
        Some("result") => {
            let errored = value
                .get("is_error")
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            let subtype = value.get("subtype").and_then(|s| s.as_str());
            Parsed::Notice(match (errored, subtype) {
                (false, _) => "run concluded".to_string(),
                (true, Some(subtype)) => format!("run ended with an error ({subtype})"),
                (true, None) => "run ended with an error".to_string(),
            })
        }
        // A record kind this parser doesn't know. Quietly nothing: the
        // stream is verbose and the shape is Claude Code's to grow.
        _ => Parsed::Nothing,
    }
}

/// A tool call's one-line label: the tool's name, plus the most identifying
/// scrap of its input the way the server labels orchestrator tool calls.
fn tool_label(item: &serde_json::Value) -> String {
    let name = item
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("tool")
        .to_string();
    let hint = item.get("input").and_then(|input| {
        [
            "command",
            "file_path",
            "path",
            "pattern",
            "query",
            "url",
            "prompt",
            "description",
        ]
        .iter()
        .find_map(|key| input.get(key).and_then(|v| v.as_str()))
    });
    match hint {
        Some(hint) => {
            let hint = hint.trim().replace('\n', " ");
            let hint: String = hint.chars().take(64).collect();
            format!("{name} · {hint}")
        }
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use tasks_client::api::models::{SessionId, TranscriptOwner};

    use super::*;

    fn line(seq: i64, text: &str) -> TranscriptLine {
        line_on(seq, text, TranscriptStream::Stdout)
    }

    fn line_on(seq: i64, text: &str, stream: TranscriptStream) -> TranscriptLine {
        TranscriptLine {
            owner: TranscriptOwner::session(&SessionId::from_raw("sess-1")),
            seq,
            timestamp: Utc.timestamp_opt(0, 0).unwrap(),
            stream,
            line: text.to_string(),
        }
    }

    fn assistant_text(seq: i64, text: &str) -> TranscriptLine {
        line(
            seq,
            &format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{text}"}}]}}}}"#
            ),
        )
    }

    fn tool_use(seq: i64, name: &str, key: &str, value: &str) -> TranscriptLine {
        line(
            seq,
            &format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"{name}","input":{{"{key}":"{value}"}}}}]}}}}"#
            ),
        )
    }

    #[test]
    fn text_renders_and_tool_calls_coalesce_between_texts() {
        let lines = [
            assistant_text(1, "reading the code"),
            tool_use(2, "Bash", "command", "ls"),
            tool_use(3, "Read", "file_path", "src/main.rs"),
            line(
                4,
                r#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#,
            ),
            tool_use(5, "Bash", "command", "cargo test"),
            assistant_text(6, "done"),
        ];
        let rows = feed_rows(&lines);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].kind, FeedRowKind::Text("reading the code".into()));
        // Tool results between calls fold away; the group keeps its first seq.
        assert_eq!(rows[1].seq, 2);
        assert_eq!(
            rows[1].kind,
            FeedRowKind::Tools(vec![
                "Bash · ls".into(),
                "Read · src/main.rs".into(),
                "Bash · cargo test".into(),
            ])
        );
        assert_eq!(rows[2].kind, FeedRowKind::Text("done".into()));
    }

    /// A growing group is the same row: its key must not change as calls
    /// coalesce in, or the list re-measures the world on every call.
    #[test]
    fn a_growing_tool_group_keeps_its_key() {
        let mut lines = vec![tool_use(2, "Bash", "command", "ls")];
        let before = feed_rows(&lines)[0].key();
        lines.push(tool_use(3, "Bash", "command", "cargo build"));
        let after = feed_rows(&lines)[0].key();
        assert_eq!(before, after);
    }

    #[test]
    fn capture_contracts_render_as_notices_not_output() {
        let lines = [
            line(1, "[tasks: truncated 40000 bytes]{\"type\":\"assistant\""),
            line(2, "[tasks] 3 transcript line(s) dropped here"),
        ];
        let rows = feed_rows(&lines);
        assert!(matches!(&rows[0].kind, FeedRowKind::Notice(n) if n.contains("truncated")));
        assert_eq!(
            rows[1].kind,
            FeedRowKind::Notice("3 transcript line(s) dropped here".into())
        );
    }

    #[test]
    fn stderr_and_non_json_render_raw_and_break_groups() {
        let lines = [
            tool_use(1, "Bash", "command", "make"),
            line_on(2, "warning: something", TranscriptStream::Stderr),
            tool_use(3, "Bash", "command", "make test"),
        ];
        let rows = feed_rows(&lines);
        assert_eq!(rows.len(), 3, "raw output must not reorder around a group");
        assert_eq!(rows[1].kind, FeedRowKind::Raw("warning: something".into()));

        let rows = feed_rows(&[line(1, "not json at all")]);
        assert_eq!(rows[0].kind, FeedRowKind::Raw("not json at all".into()));
    }

    #[test]
    fn lifecycle_records_are_quiet_notices() {
        let lines = [
            line(
                1,
                r#"{"type":"system","subtype":"init","model":"claude-opus-5"}"#,
            ),
            line(
                2,
                r#"{"type":"result","subtype":"success","is_error":false}"#,
            ),
            line(
                3,
                r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#,
            ),
        ];
        let rows = feed_rows(&lines);
        assert_eq!(
            rows[0].kind,
            FeedRowKind::Notice("session started · claude-opus-5".into())
        );
        assert_eq!(rows[1].kind, FeedRowKind::Notice("run concluded".into()));
        assert!(matches!(&rows[2].kind, FeedRowKind::Notice(n) if n.contains("error_max_turns")));
    }

    /// Unknown record kinds are the stream growing under us — skipped, not
    /// rendered raw, because they *are* valid records.
    #[test]
    fn unknown_records_and_empty_lines_render_as_nothing() {
        let lines = [
            line(1, r#"{"type":"stream_event","event":{}}"#),
            line_on(2, "   ", TranscriptStream::Stderr),
            line(3, r#"{"type":"system","subtype":"status"}"#),
        ];
        assert!(feed_rows(&lines).is_empty());
    }

    #[test]
    fn tool_labels_carry_the_identifying_scrap() {
        let long = "x".repeat(200);
        let lines = [tool_use(1, "Bash", "command", &long)];
        let FeedRowKind::Tools(labels) = &feed_rows(&lines)[0].kind else {
            panic!("expected tools");
        };
        assert!(labels[0].starts_with("Bash · xxx"));
        assert!(labels[0].len() < 80, "hint must be truncated");
    }
}
