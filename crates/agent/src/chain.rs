//! Chain execution for sequential prompt and programmatic steps.
//!
//! A chain is a sequence of steps where each step can be:
//! - An LLM prompt that gets a response
//! - A programmatic transform that modifies data
//! - A gate/validator that can pass or fail, stopping the chain

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::AgentError;
use crate::message::{Message, Response, Tool, ToolCall, Usage};
use crate::provider::{CompletionConfig, Provider};
use crate::session::{Session, SessionState};

/// Result of a chain step execution.
#[derive(Debug, Clone)]
pub enum StepResult {
    Continue(StepOutput),
    Stop(String),
    NeedsToolResults(Vec<ToolCall>),
}

/// Output from a chain step.
#[derive(Debug, Clone)]
pub struct StepOutput {
    pub text: String,
    pub response: Option<Response>,
    pub metadata: serde_json::Value,
}

impl Default for StepOutput {
    fn default() -> Self {
        Self { text: String::new(), response: None, metadata: serde_json::Value::Null }
    }
}

impl StepOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Default::default() }
    }

    pub fn from_response(response: Response) -> Self {
        Self { text: response.text(), response: Some(response), metadata: serde_json::Value::Null }
    }
}

pub type Validator = Box<dyn Fn(&StepOutput) -> Result<(), String> + Send + Sync>;
pub type Transform = Box<dyn Fn(StepOutput) -> StepOutput + Send + Sync>;

pub enum ChainStep<P: Provider> {
    Prompt { message: String, validator: Option<Validator> },
    Transform { name: String, transform: Transform },
    Gate { name: String, validator: Validator },
    Custom {
        name: String,
        func: Box<dyn Fn(StepOutput, Arc<P>) -> Pin<Box<dyn Future<Output = Result<StepResult, AgentError>> + Send>> + Send + Sync>,
    },
}

pub struct ChainBuilder<P: Provider> {
    provider: Arc<P>,
    session: Session,
    steps: Vec<ChainStep<P>>,
}

impl<P: Provider> ChainBuilder<P> {
    pub fn new(provider: Arc<P>, model: impl Into<String>) -> Self {
        Self {
            provider,
            session: Session::new(CompletionConfig::new(model)),
            steps: Vec::new(),
        }
    }

    pub fn system(mut self, prompt: impl Into<String>) -> Self {
        self.session.set_system_prompt(prompt);
        self
    }

    pub fn tools(mut self, tools: Vec<Tool>) -> Self {
        self.session.set_tools(tools);
        self
    }

    pub fn prompt(mut self, message: impl Into<String>) -> Self {
        self.steps.push(ChainStep::Prompt { message: message.into(), validator: None });
        self
    }

    pub fn prompt_validated<F>(mut self, message: impl Into<String>, validator: F) -> Self
    where
        F: Fn(&StepOutput) -> Result<(), String> + Send + Sync + 'static,
    {
        self.steps.push(ChainStep::Prompt {
            message: message.into(),
            validator: Some(Box::new(validator)),
        });
        self
    }

    pub fn transform<F>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: Fn(StepOutput) -> StepOutput + Send + Sync + 'static,
    {
        self.steps.push(ChainStep::Transform { name: name.into(), transform: Box::new(f) });
        self
    }

    pub fn gate<F>(mut self, name: impl Into<String>, validator: F) -> Self
    where
        F: Fn(&StepOutput) -> Result<(), String> + Send + Sync + 'static,
    {
        self.steps.push(ChainStep::Gate { name: name.into(), validator: Box::new(validator) });
        self
    }

    pub fn custom<F, Fut>(mut self, name: impl Into<String>, f: F) -> Self
    where
        F: Fn(StepOutput, Arc<P>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<StepResult, AgentError>> + Send + 'static,
    {
        let name = name.into();
        self.steps.push(ChainStep::Custom {
            name,
            func: Box::new(move |output, provider| Box::pin(f(output, provider))),
        });
        self
    }

    pub async fn run(mut self) -> Result<ChainResult, AgentError> {
        let mut current_output = StepOutput::default();
        let mut executed_steps = Vec::new();
        let steps = std::mem::take(&mut self.steps);

        for (idx, step) in steps.into_iter().enumerate() {
            let step_name = match &step {
                ChainStep::Prompt { message, .. } => {
                    let truncated: String = message.chars().take(30).collect();
                    format!("prompt_{}: {}", idx, truncated)
                }
                ChainStep::Transform { name, .. } => format!("transform: {}", name),
                ChainStep::Gate { name, .. } => format!("gate: {}", name),
                ChainStep::Custom { name, .. } => format!("custom: {}", name),
            };

            let result = self.execute_step(step, current_output.clone()).await?;

            match result {
                StepResult::Continue(output) => {
                    executed_steps.push((step_name, true));
                    current_output = output;
                }
                StepResult::Stop(reason) => {
                    executed_steps.push((step_name, false));
                    let total_usage = self.session.total_usage;
                    return Ok(ChainResult {
                        output: current_output,
                        session: self.session,
                        executed_steps,
                        stopped_reason: Some(reason),
                        total_usage,
                    });
                }
                StepResult::NeedsToolResults(tool_calls) => {
                    executed_steps.push((step_name, false));
                    let total_usage = self.session.total_usage;
                    return Ok(ChainResult {
                        output: current_output,
                        session: self.session,
                        executed_steps,
                        stopped_reason: Some(format!(
                            "Chain stopped: needs tool results for {:?}",
                            tool_calls.iter().map(|t| &t.name).collect::<Vec<_>>()
                        )),
                        total_usage,
                    });
                }
            }
        }

        let total_usage = self.session.total_usage;
        Ok(ChainResult {
            output: current_output,
            session: self.session,
            executed_steps,
            stopped_reason: None,
            total_usage,
        })
    }

    async fn execute_step(
        &mut self,
        step: ChainStep<P>,
        current_output: StepOutput,
    ) -> Result<StepResult, AgentError> {
        match step {
            ChainStep::Prompt { message, validator } => {
                self.session.add_user_message(&message);
                self.session.state = SessionState::Processing;
                self.session.compact_if_needed(self.provider.as_ref()).await?;
                let request = self.session.build_request();
                let response = self.provider.complete(request).await?;

                if !response.tool_calls.is_empty() {
                    self.session.apply_response(&response);
                    return Ok(StepResult::NeedsToolResults(response.tool_calls.clone()));
                }

                self.session.apply_response(&response);
                let output = StepOutput::from_response(response);

                if let Some(validator) = validator {
                    if let Err(reason) = validator(&output) {
                        return Ok(StepResult::Stop(reason));
                    }
                }

                Ok(StepResult::Continue(output))
            }
            ChainStep::Transform { transform, .. } => {
                Ok(StepResult::Continue(transform(current_output)))
            }
            ChainStep::Gate { validator, .. } => {
                if let Err(reason) = validator(&current_output) {
                    Ok(StepResult::Stop(reason))
                } else {
                    Ok(StepResult::Continue(current_output))
                }
            }
            ChainStep::Custom { func, .. } => {
                func(current_output, Arc::clone(&self.provider)).await
            }
        }
    }
}

/// Result of executing a chain.
#[derive(Debug)]
pub struct ChainResult {
    pub output: StepOutput,
    pub session: Session,
    pub executed_steps: Vec<(String, bool)>,
    pub stopped_reason: Option<String>,
    pub total_usage: Usage,
}

impl ChainResult {
    pub fn succeeded(&self) -> bool {
        self.stopped_reason.is_none()
    }

    pub fn text(&self) -> &str {
        &self.output.text
    }

    pub fn history(&self) -> &[Message] {
        self.session.history()
    }
}
