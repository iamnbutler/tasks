//! Anthropic Claude provider implementation with streaming support.

use bytes::Bytes;
use futures::Stream;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::error::AgentError;
use crate::message::{Content, Response, StopReason, ToolCall, Usage};
use crate::provider::{CompletionRequest, Provider, StreamChunk};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Claude provider.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    api_key: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self { api_key: api_key.into(), client: reqwest::Client::new() }
    }

    pub fn from_env() -> Result<Self, AgentError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| AgentError::Auth("ANTHROPIC_API_KEY environment variable not set".into()))?;
        Ok(Self::new(api_key))
    }

    fn build_headers(&self) -> Result<HeaderMap, AgentError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&self.api_key)
                .map_err(|_| AgentError::Auth("Invalid API key".into()))?,
        );
        headers.insert("anthropic-version", HeaderValue::from_static(ANTHROPIC_VERSION));
        Ok(headers)
    }
}

// --- Anthropic API types (private) ---

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<AnthropicContent>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContent {
    Text { text: String },
    Image { source: ImageSource },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: String, #[serde(default, skip_serializing_if = "std::ops::Not::not")] is_error: bool },
}

#[derive(Debug, Serialize, Deserialize)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicResponseContent>,
    stop_reason: Option<String>,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicResponseContent {
    Text { text: String },
    Thinking { thinking: String },
    ToolUse { id: String, name: String, input: serde_json::Value },
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct AnthropicError {
    error: AnthropicErrorDetail,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    error_type: String,
    message: String,
}

// --- Streaming SSE types ---

#[derive(Debug, Deserialize)]
struct StreamMessageStart { message: StreamMessageStartMessage }

#[derive(Debug, Deserialize)]
struct StreamMessageStartMessage { usage: StreamUsage }

#[derive(Debug, Deserialize)]
struct StreamUsage {
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct StreamContentBlockStart { content_block: StreamContentBlock }

#[derive(Debug, Deserialize)]
struct StreamContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamContentBlockDelta { delta: StreamDelta }

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(rename = "type")]
    delta_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamMessageDelta {
    delta: StreamMessageDeltaContent,
    #[serde(default)]
    usage: Option<StreamUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamMessageDeltaContent {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamError { error: StreamErrorDetail }

#[derive(Debug, Deserialize)]
struct StreamErrorDetail { message: String }

// --- Message conversion ---

impl From<&crate::message::Message> for AnthropicMessage {
    fn from(msg: &crate::message::Message) -> Self {
        let role = match msg.role {
            crate::message::Role::User => "user",
            crate::message::Role::Assistant => "assistant",
            crate::message::Role::System => "user",
        };

        let content = msg.content.iter().filter_map(|c| match c {
            Content::Text { text } => Some(AnthropicContent::Text { text: text.clone() }),
            // Thinking blocks are internal reasoning from prior turns — filter
            // them out rather than replaying as plain text, which would confuse
            // the model.
            Content::Thinking { .. } => None,
            Content::Image { media_type, data } => Some(AnthropicContent::Image {
                source: ImageSource {
                    source_type: "base64".to_string(),
                    media_type: media_type.clone(),
                    data: data.clone(),
                },
            }),
            Content::ToolUse { id, name, input } => Some(AnthropicContent::ToolUse {
                id: id.clone(), name: name.clone(), input: input.clone(),
            }),
            Content::ToolResult { tool_use_id, content, is_error } => Some(AnthropicContent::ToolResult {
                tool_use_id: tool_use_id.clone(), content: content.clone(), is_error: *is_error,
            }),
        }).collect();

        AnthropicMessage { role: role.to_string(), content }
    }
}

// --- SSE stream processing ---

async fn process_sse_stream<S>(stream: S, tx: mpsc::UnboundedSender<StreamChunk>)
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    use futures::StreamExt;

    let mut stream = stream;
    let mut buffer = String::new();
    let mut content: Vec<Content> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage = Usage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut current_text = String::new();
    let mut current_thinking = String::new();
    let mut current_tool_id = String::new();
    let mut current_tool_name = String::new();
    let mut current_tool_input = String::new();
    let mut current_block_type: Option<String> = None;

    while let Some(chunk_result) = stream.next().await {
        let chunk = match chunk_result {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(StreamChunk::Error(e.to_string()));
                return;
            }
        };

        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(event_end) = buffer.find("\n\n") {
            let event_data = buffer[..event_end].to_string();
            buffer = buffer[event_end + 2..].to_string();

            let mut event_type = String::new();
            let mut data = String::new();

            for line in event_data.lines() {
                if let Some(et) = line.strip_prefix("event: ") {
                    event_type = et.to_string();
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data = d.to_string();
                }
            }

            if data.is_empty() { continue; }

            match event_type.as_str() {
                "message_start" => {
                    if let Ok(msg) = serde_json::from_str::<StreamMessageStart>(&data) {
                        usage.input_tokens = msg.message.usage.input_tokens;
                    }
                }
                "content_block_start" => {
                    if let Ok(block) = serde_json::from_str::<StreamContentBlockStart>(&data) {
                        current_block_type = Some(block.content_block.block_type.clone());
                        match block.content_block.block_type.as_str() {
                            "text" => current_text.clear(),
                            "thinking" => current_thinking.clear(),
                            "tool_use" => {
                                current_tool_id = block.content_block.id.unwrap_or_default();
                                current_tool_name = block.content_block.name.unwrap_or_default();
                                current_tool_input.clear();
                                let _ = tx.send(StreamChunk::ToolUseStart {
                                    id: current_tool_id.clone(),
                                    name: current_tool_name.clone(),
                                });
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_delta" => {
                    if let Ok(delta) = serde_json::from_str::<StreamContentBlockDelta>(&data) {
                        match delta.delta.delta_type.as_str() {
                            "text_delta" => {
                                if let Some(text) = delta.delta.text {
                                    current_text.push_str(&text);
                                    let _ = tx.send(StreamChunk::Text(text));
                                }
                            }
                            "thinking_delta" => {
                                if let Some(thinking) = delta.delta.thinking {
                                    current_thinking.push_str(&thinking);
                                    let _ = tx.send(StreamChunk::Thinking(thinking));
                                }
                            }
                            "input_json_delta" => {
                                if let Some(partial) = delta.delta.partial_json {
                                    current_tool_input.push_str(&partial);
                                    let _ = tx.send(StreamChunk::ToolUseInput(partial));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_stop" => {
                    if let Some(ref block_type) = current_block_type {
                        match block_type.as_str() {
                            "text" => {
                                if !current_text.is_empty() {
                                    content.push(Content::text(current_text.clone()));
                                }
                            }
                            "thinking" => {
                                if !current_thinking.is_empty() {
                                    content.push(Content::thinking(current_thinking.clone()));
                                }
                            }
                            "tool_use" => {
                                let input: serde_json::Value = serde_json::from_str(&current_tool_input)
                                    .unwrap_or(serde_json::Value::Null);
                                content.push(Content::ToolUse {
                                    id: current_tool_id.clone(),
                                    name: current_tool_name.clone(),
                                    input: input.clone(),
                                });
                                tool_calls.push(ToolCall {
                                    id: current_tool_id.clone(),
                                    name: current_tool_name.clone(),
                                    arguments: input,
                                });
                            }
                            _ => {}
                        }
                    }
                    current_block_type = None;
                }
                "message_delta" => {
                    if let Ok(delta) = serde_json::from_str::<StreamMessageDelta>(&data) {
                        stop_reason = delta.delta.stop_reason.map(|r| match r.as_str() {
                            "end_turn" => StopReason::EndTurn,
                            "max_tokens" => StopReason::MaxTokens,
                            "tool_use" => StopReason::ToolUse,
                            "stop_sequence" => StopReason::StopSequence,
                            _ => StopReason::EndTurn,
                        });
                        if let Some(u) = delta.usage {
                            usage.output_tokens = u.output_tokens;
                        }
                    }
                }
                "message_stop" => {
                    let response = Response {
                        content: content.clone(),
                        tool_calls: tool_calls.clone(),
                        stop_reason,
                        usage: Some(usage),
                    };
                    let _ = tx.send(StreamChunk::Complete(response));
                    return;
                }
                "error" => {
                    if let Ok(err) = serde_json::from_str::<StreamError>(&data) {
                        let _ = tx.send(StreamChunk::Error(err.error.message));
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    let response = Response { content, tool_calls, stop_reason, usage: Some(usage) };
    let _ = tx.send(StreamChunk::Complete(response));
}

// --- Provider implementation ---

impl Provider for AnthropicProvider {
    fn name(&self) -> &str { "anthropic" }

    fn models(&self) -> &[&str] {
        &[
            "claude-sonnet-4-20250514",
            "claude-opus-4-0-20250514",
            "claude-haiku-4-5-20251001",
            "claude-3-5-haiku-20241022",
        ]
    }

    fn is_available(&self) -> bool { !self.api_key.is_empty() }

    fn supports_streaming(&self) -> bool { true }

    async fn complete(&self, request: CompletionRequest) -> std::result::Result<Response, AgentError> {
        let headers = self.build_headers()?;

        let messages: Vec<AnthropicMessage> = request.messages.iter()
            .filter(|m| m.role != crate::message::Role::System)
            .map(AnthropicMessage::from)
            .collect();

        let tools: Vec<AnthropicTool> = request.tools.iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.parameters.clone(),
            })
            .collect();

        let anthropic_request = AnthropicRequest {
            model: request.config.model,
            max_tokens: request.config.max_tokens,
            system: request.system,
            messages,
            temperature: request.config.temperature,
            top_p: request.config.top_p,
            stop_sequences: request.config.stop_sequences,
            tools,
            stream: false,
        };

        let response = self.client
            .post(ANTHROPIC_API_URL)
            .headers(headers)
            .json(&anthropic_request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let error_body: AnthropicError = response.json().await.map_err(|e| {
                AgentError::api(status.as_u16(), format!("Failed to parse error response: {}", e))
            })?;
            return match status.as_u16() {
                401 => Err(AgentError::Auth(error_body.error.message)),
                429 => Err(AgentError::RateLimited { retry_after: None }),
                _ => Err(AgentError::api(status.as_u16(), error_body.error.message)),
            };
        }

        let anthropic_response: AnthropicResponse = response.json().await?;

        let mut content = Vec::new();
        let mut tool_calls = Vec::new();

        for item in anthropic_response.content {
            match item {
                AnthropicResponseContent::Text { text } => {
                    content.push(Content::text(text));
                }
                AnthropicResponseContent::Thinking { thinking } => {
                    content.push(Content::thinking(thinking));
                }
                AnthropicResponseContent::ToolUse { id, name, input } => {
                    content.push(Content::ToolUse {
                        id: id.clone(), name: name.clone(), input: input.clone(),
                    });
                    tool_calls.push(ToolCall { id, name, arguments: input });
                }
            }
        }

        let stop_reason = anthropic_response.stop_reason.map(|r| match r.as_str() {
            "end_turn" => StopReason::EndTurn,
            "max_tokens" => StopReason::MaxTokens,
            "tool_use" => StopReason::ToolUse,
            "stop_sequence" => StopReason::StopSequence,
            _ => StopReason::EndTurn,
        });

        Ok(Response {
            content,
            tool_calls,
            stop_reason,
            usage: Some(Usage {
                input_tokens: anthropic_response.usage.input_tokens,
                output_tokens: anthropic_response.usage.output_tokens,
            }),
        })
    }

    async fn complete_streaming(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<mpsc::UnboundedReceiver<StreamChunk>, AgentError> {
        let headers = self.build_headers()?;

        let messages: Vec<AnthropicMessage> = request.messages.iter()
            .filter(|m| m.role != crate::message::Role::System)
            .map(AnthropicMessage::from)
            .collect();

        let tools: Vec<AnthropicTool> = request.tools.iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.parameters.clone(),
            })
            .collect();

        let anthropic_request = AnthropicRequest {
            model: request.config.model,
            max_tokens: request.config.max_tokens,
            system: request.system,
            messages,
            temperature: request.config.temperature,
            top_p: request.config.top_p,
            stop_sequences: request.config.stop_sequences,
            tools,
            stream: true,
        };

        let response = self.client
            .post(ANTHROPIC_API_URL)
            .headers(headers)
            .json(&anthropic_request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            if let Ok(error_body) = serde_json::from_str::<AnthropicError>(&error_text) {
                return match status.as_u16() {
                    401 => Err(AgentError::Auth(error_body.error.message)),
                    429 => Err(AgentError::RateLimited { retry_after: None }),
                    _ => Err(AgentError::api(status.as_u16(), error_body.error.message)),
                };
            }
            return Err(AgentError::api(status.as_u16(), error_text));
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let byte_stream = response.bytes_stream();
        tokio::spawn(async move {
            process_sse_stream(byte_stream, tx).await;
        });

        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, Role, Content};

    #[test]
    fn test_message_conversion_user() {
        let msg = Message::user("hello");
        let anthropic: AnthropicMessage = (&msg).into();
        assert_eq!(anthropic.role, "user");
        assert_eq!(anthropic.content.len(), 1);
    }

    #[test]
    fn test_message_conversion_assistant() {
        let msg = Message::assistant("response");
        let anthropic: AnthropicMessage = (&msg).into();
        assert_eq!(anthropic.role, "assistant");
    }

    #[test]
    fn test_message_conversion_system_becomes_user() {
        let msg = Message::system("you are helpful");
        let anthropic: AnthropicMessage = (&msg).into();
        assert_eq!(anthropic.role, "user");
    }

    #[test]
    fn test_message_conversion_tool_use() {
        let msg = Message::new(Role::Assistant, vec![
            Content::ToolUse {
                id: "tool-1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "/tmp/test"}),
            },
        ]);
        let anthropic: AnthropicMessage = (&msg).into();
        assert_eq!(anthropic.content.len(), 1);
        match &anthropic.content[0] {
            AnthropicContent::ToolUse { id, name, .. } => {
                assert_eq!(id, "tool-1");
                assert_eq!(name, "read_file");
            }
            _ => panic!("Expected ToolUse"),
        }
    }

    #[test]
    fn test_from_env_missing_key() {
        let result = AnthropicProvider::from_env();
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            assert!(result.is_err());
        }
    }
}
