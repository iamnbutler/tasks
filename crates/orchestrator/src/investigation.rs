//! Investigation agents for deep PR evaluation.
//!
//! When the orchestrator's triage pass identifies concerns during PR review,
//! it can spawn lightweight investigation agents to dig deeper. Each agent
//! is a focused, read-only LLM call that answers a specific question about
//! the PR — e.g., checking test coverage, tracing lifecycle patterns, or
//! verifying issue alignment.
//!
//! Investigations run in parallel and feed their findings back into the
//! orchestrator's final evaluation decision.

use futures::future::join_all;
use tracing::{info, warn};

use crate::types::{InvestigationRequest, InvestigationResult};
use tasks_agent::{AnthropicProvider, CompletionConfig, CompletionRequest, Message, Provider};
use tasks_github::GitHubClient;

/// Maximum number of investigations that can run in parallel per evaluation.
const MAX_INVESTIGATIONS: usize = 5;

/// Maximum tokens for investigation agent responses (kept short for speed).
const INVESTIGATION_MAX_TOKENS: u32 = 4096;

/// Maximum files to fetch per investigation.
const MAX_FILES_PER_INVESTIGATION: usize = 3;

/// Run a set of investigation agents in parallel, returning their results.
///
/// Each investigation gets its own LLM call with focused context. Files
/// requested by the investigation are fetched from GitHub and included.
/// Investigations that fail (API errors, timeouts) are reported as errors
/// in their findings rather than failing the entire evaluation.
pub async fn run_investigations(
    provider: &AnthropicProvider,
    github: &GitHubClient,
    model: &str,
    owner: &str,
    repo: &str,
    head_ref: &str,
    diff: Option<&str>,
    requests: Vec<InvestigationRequest>,
) -> Vec<InvestigationResult> {
    let requests: Vec<_> = requests.into_iter().take(MAX_INVESTIGATIONS).collect();

    info!(
        count = requests.len(),
        "Running investigation agents in parallel"
    );

    let futures: Vec<_> = requests
        .into_iter()
        .map(|request| {
            run_single_investigation(provider, github, model, owner, repo, head_ref, diff, request)
        })
        .collect();

    join_all(futures).await
}

/// Run a single investigation agent.
async fn run_single_investigation(
    provider: &AnthropicProvider,
    github: &GitHubClient,
    model: &str,
    owner: &str,
    repo: &str,
    head_ref: &str,
    diff: Option<&str>,
    request: InvestigationRequest,
) -> InvestigationResult {
    info!(title = %request.title, "Starting investigation");

    // Fetch requested files
    let mut file_contents: Vec<(String, String)> = Vec::new();
    for file_path in request.files.iter().take(MAX_FILES_PER_INVESTIGATION) {
        match github
            .get_file_content_at_ref(owner, repo, file_path, head_ref)
            .await
        {
            Ok(Some(content)) => {
                info!(path = %file_path, len = content.len(), "Fetched file for investigation");
                file_contents.push((file_path.clone(), content));
            }
            Ok(None) => {
                info!(path = %file_path, "File not found on PR branch");
                file_contents.push((file_path.clone(), "(file not found)".to_string()));
            }
            Err(e) => {
                warn!(path = %file_path, error = %e, "Failed to fetch file for investigation");
                file_contents.push((file_path.clone(), format!("(fetch error: {e})")));
            }
        }
    }

    // Build the investigation prompt
    let system = investigation_system_prompt();
    let user_prompt = build_investigation_prompt(&request, diff, &file_contents);

    let config = CompletionConfig::new(model).with_max_tokens(INVESTIGATION_MAX_TOKENS);
    let llm_request = CompletionRequest::new(config, vec![Message::user(user_prompt)])
        .with_system(system);

    match provider.complete(llm_request).await {
        Ok(response) => {
            let text = response.text();
            info!(
                title = %request.title,
                response_len = text.len(),
                "Investigation complete"
            );
            parse_investigation_response(&request, &text)
        }
        Err(e) => {
            warn!(title = %request.title, error = %e, "Investigation failed");
            InvestigationResult {
                request,
                finding: format!("Investigation failed: {e}"),
                concern: false,
            }
        }
    }
}

/// System prompt for investigation agents.
fn investigation_system_prompt() -> String {
    r#"You are an investigation agent performing a focused code review inquiry. You are read-only — you do not make changes, only investigate and report.

Your job:
1. Answer the specific question you've been given
2. Be thorough but concise — focus on facts, not speculation
3. Cite specific files, functions, and line references when possible
4. State clearly whether you found a concern that should block the PR

Response format (JSON):
{
  "finding": "Your detailed findings answering the investigation question",
  "concern": true|false
}

Set "concern" to true ONLY if you found a concrete issue that should block merge — not for stylistic preferences or hypothetical problems. If you're unsure, lean toward false and explain your uncertainty in the finding."#.to_string()
}

/// Build the user prompt for an investigation agent.
fn build_investigation_prompt(
    request: &InvestigationRequest,
    diff: Option<&str>,
    files: &[(String, String)],
) -> String {
    let mut prompt = String::new();

    prompt.push_str(&format!("## Investigation: {}\n\n", request.title));
    prompt.push_str(&format!("**Question:** {}\n\n", request.question));

    if let Some(diff) = diff {
        prompt.push_str("## PR Diff\n\n```diff\n");
        // Truncate diff for investigations to keep context focused
        let max_diff = 15_000;
        if diff.len() > max_diff {
            prompt.push_str(&diff[..max_diff]);
            prompt.push_str("\n... (truncated)\n");
        } else {
            prompt.push_str(diff);
        }
        if !diff.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("```\n\n");
    }

    if !files.is_empty() {
        prompt.push_str("## Files\n\n");
        for (path, content) in files {
            prompt.push_str(&format!("### `{}`\n\n```\n", path));
            // Truncate individual files
            let max_file = 10_000;
            if content.len() > max_file {
                prompt.push_str(&content[..max_file]);
                prompt.push_str("\n... (truncated)\n");
            } else {
                prompt.push_str(content);
            }
            prompt.push_str("\n```\n\n");
        }
    }

    prompt.push_str("## Instructions\n\n");
    prompt.push_str("Answer the investigation question above. ");
    prompt.push_str("Be specific and cite evidence from the code. ");
    prompt.push_str("Respond with JSON in the format specified in your instructions.");

    prompt
}

/// Parse an investigation agent's response into an InvestigationResult.
fn parse_investigation_response(
    request: &InvestigationRequest,
    text: &str,
) -> InvestigationResult {
    // Try to extract JSON from the response
    if let Some(json_str) = extract_json(text) {
        if let Ok(parsed) = serde_json::from_str::<InvestigationResponse>(json_str) {
            return InvestigationResult {
                request: request.clone(),
                finding: parsed.finding,
                concern: parsed.concern,
            };
        }
    }

    // Fallback: treat the entire response as the finding
    InvestigationResult {
        request: request.clone(),
        finding: text.trim().to_string(),
        concern: false,
    }
}

/// Internal struct for deserializing investigation agent JSON responses.
#[derive(serde::Deserialize)]
struct InvestigationResponse {
    finding: String,
    #[serde(default)]
    concern: bool,
}

/// Extract JSON from a text response (same logic as claude.rs).
fn extract_json(text: &str) -> Option<&str> {
    // Try code block first
    if let Some(start) = text.find("```json") {
        let json_start = start + 7;
        if let Some(end) = text[json_start..].find("```") {
            return Some(text[json_start..json_start + end].trim());
        }
    }

    // Try generic code block
    if let Some(start) = text.find("```") {
        let block_start = start + 3;
        let content_start = text[block_start..]
            .find('\n')
            .map(|i| block_start + i + 1)
            .unwrap_or(block_start);
        if let Some(end) = text[content_start..].find("```") {
            let candidate = text[content_start..content_start + end].trim();
            if candidate.starts_with('{') {
                return Some(candidate);
            }
        }
    }

    // Try raw JSON
    if let Some(start) = text.find('{') {
        let mut depth = 0;
        let mut in_string = false;
        let mut escape_next = false;

        for (i, c) in text[start..].char_indices() {
            if escape_next {
                escape_next = false;
                continue;
            }
            match c {
                '\\' if in_string => escape_next = true,
                '"' => in_string = !in_string,
                '{' if !in_string => depth += 1,
                '}' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&text[start..start + i + 1]);
                    }
                }
                _ => {}
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_investigation_response_json() {
        let text = r#"```json
{"finding": "Tests cover the main path but miss the timeout edge case", "concern": true}
```"#;
        let request = InvestigationRequest {
            title: "test coverage".to_string(),
            question: "Do tests cover edge cases?".to_string(),
            files: vec![],
        };
        let result = parse_investigation_response(&request, text);
        assert!(result.concern);
        assert!(result.finding.contains("timeout edge case"));
    }

    #[test]
    fn test_parse_investigation_response_raw_json() {
        let text = r#"{"finding": "No issues found", "concern": false}"#;
        let request = InvestigationRequest {
            title: "check".to_string(),
            question: "Any issues?".to_string(),
            files: vec![],
        };
        let result = parse_investigation_response(&request, text);
        assert!(!result.concern);
        assert_eq!(result.finding, "No issues found");
    }

    #[test]
    fn test_parse_investigation_response_fallback() {
        let text = "I couldn't find any JSON but the code looks fine.";
        let request = InvestigationRequest {
            title: "check".to_string(),
            question: "Any issues?".to_string(),
            files: vec![],
        };
        let result = parse_investigation_response(&request, text);
        assert!(!result.concern);
        assert_eq!(result.finding, text);
    }

    #[test]
    fn test_build_investigation_prompt_includes_question() {
        let request = InvestigationRequest {
            title: "test coverage".to_string(),
            question: "Does the rate limiter have edge case tests?".to_string(),
            files: vec!["src/rate_limiter_test.rs".to_string()],
        };
        let files = vec![("src/rate_limiter_test.rs".to_string(), "fn test_basic() {}".to_string())];
        let prompt = build_investigation_prompt(&request, Some("diff content"), &files);

        assert!(prompt.contains("test coverage"));
        assert!(prompt.contains("rate limiter have edge case tests"));
        assert!(prompt.contains("rate_limiter_test.rs"));
        assert!(prompt.contains("fn test_basic()"));
        assert!(prompt.contains("diff content"));
    }

    #[test]
    fn test_build_investigation_prompt_no_diff() {
        let request = InvestigationRequest {
            title: "check".to_string(),
            question: "Is it safe?".to_string(),
            files: vec![],
        };
        let prompt = build_investigation_prompt(&request, None, &[]);
        assert!(!prompt.contains("## PR Diff"));
        assert!(prompt.contains("Is it safe?"));
    }

    #[test]
    fn test_investigation_system_prompt_contains_key_instructions() {
        let prompt = investigation_system_prompt();
        assert!(prompt.contains("read-only"));
        assert!(prompt.contains("concern"));
        assert!(prompt.contains("JSON"));
    }
}
