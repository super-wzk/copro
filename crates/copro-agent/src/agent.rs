use crate::event::{AgentEvent, AgentStream};
use crate::hook::{AgentHook, ToolDecision};
use crate::tools::ToolRouter;
use copro_api::error::{Error, Result};
use copro_api::message::{
    InputContent, Message, OutputContent, ToolCall, ToolResult, ToolResultStatus,
};
use copro_api::request::{GenerateRequest, GenerateRequestOptions};
use copro_api::stream::{Model, ModelStream, OutputStreamEvent, OutputStreamState};
use futures_util::StreamExt;
use std::sync::Arc;

/// Conversational agent bound to one model with tools, hooks, and conversation state.
pub struct Agent {
    pub model: Arc<dyn Model>,
    pub tools: Arc<dyn ToolRouter>,
    pub hooks: Vec<Arc<dyn AgentHook>>,
    pub max_tool_rounds: usize,
    pub messages: Vec<Message>,
}

impl Agent {
    pub fn new(model: Arc<dyn Model>, tools: Arc<dyn ToolRouter>) -> Self {
        Self {
            model,
            tools,
            hooks: Vec::new(),
            max_tool_rounds: 10,
            messages: Vec::new(),
        }
    }

    /// Run one streaming turn using this agent's bound model and conversation state.
    ///
    /// Model content is streamed as [`AgentEvent::OutputDelta`] events.
    /// Completed model outputs and tool results are yielded when they are
    /// committed to state. Stream completion marks the end of the turn.
    pub fn run_stream(&mut self) -> AgentStream<'_> {
        Box::pin(async_stream::try_stream! {
            let model = Arc::clone(&self.model);
            let mut state = TurnState::ModelRequest;
            let mut model_rounds = 0usize;

            loop {
                state = match state {
                    TurnState::ModelRequest => {
                        if model_rounds >= self.max_tool_rounds {
                            Err(Error::client("max tool rounds exceeded"))?;
                        }
                        model_rounds += 1;

                        let mut request = self.build_request(self.messages.clone()).await?;
                        self.apply_before_request(&mut request).await?;
                        TurnState::ModelStreaming {
                            stream: model.stream(request),
                            output_state: OutputStreamState::new(),
                        }
                    }
                    TurnState::ModelStreaming {
                        mut stream,
                        mut output_state,
                    } => {
                        let event = stream
                            .next()
                            .await
                            .transpose()?
                            .ok_or_else(|| Error::protocol("stream ended before finished event"))?;

                        match event {
                            OutputStreamEvent::Delta {
                                content_index,
                                delta,
                            } => {
                                yield AgentEvent::OutputDelta(delta.clone());
                                output_state.apply(OutputStreamEvent::Delta {
                                    content_index,
                                    delta,
                                })?;
                                TurnState::ModelStreaming {
                                    stream,
                                    output_state,
                                }
                            }
                            OutputStreamEvent::Finished { reason, usage } => {
                                let response = output_state
                                    .apply(OutputStreamEvent::Finished { reason, usage })?
                                    .ok_or_else(|| {
                                        Error::protocol("stream ended before finished event")
                                    })?;

                                let usage = response.usage.clone();
                                let mut output_content = into_assistant_content(response.message)?;
                                self.apply_before_output_commit(&mut output_content).await?;
                                self.messages.push(normalize_for_history(Message::Assistant(
                                    output_content.clone(),
                                )));
                                self.apply_after_output_commit(&output_content).await?;
                                yield AgentEvent::OutputFinished {
                                    content: output_content.clone(),
                                    reason,
                                    usage,
                                };

                                if output_content
                                    .iter()
                                    .any(|content| matches!(content, OutputContent::ToolCall(_)))
                                {
                                    TurnState::ToolExecution { output_content }
                                } else {
                                    TurnState::Finished
                                }
                            }
                        }
                    }
                    TurnState::ToolExecution { output_content } => {
                        for item in output_content {
                            let OutputContent::ToolCall(mut tool) = item else {
                                continue;
                            };
                            let mut result = match self.apply_before_tool_call(&mut tool).await? {
                                ToolDecision::Allow => self.tools.execute(tool.clone()).await?,
                                ToolDecision::Reject { reason } => ToolResult {
                                    call_id: tool.id.clone(),
                                    name: tool.name.clone(),
                                    status: ToolResultStatus::Error,
                                    content: vec![InputContent::Text(reason)],
                                },
                            };
                            self.apply_after_tool_call(&tool, &mut result).await?;
                            self.messages.push(Message::Tool(result.clone()));
                            yield AgentEvent::ToolResult(result);
                        }

                        TurnState::ModelRequest
                    }
                    TurnState::Finished => break,
                };
            }
        })
    }

    async fn build_request(&self, messages: Vec<Message>) -> Result<GenerateRequest> {
        Ok(GenerateRequest {
            messages,
            tools: self.tools.definitions().await?,
            hosted_tools: Vec::new(),
            tool_choice: None,
            options: GenerateRequestOptions::default(),
        })
    }

    async fn apply_before_request(&self, request: &mut GenerateRequest) -> Result<()> {
        for hook in &self.hooks {
            hook.before_request(request).await?;
        }
        Ok(())
    }

    async fn apply_before_tool_call(&self, tool: &mut ToolCall) -> Result<ToolDecision> {
        for hook in &self.hooks {
            match hook.before_tool_call(tool).await? {
                ToolDecision::Allow => {}
                decision => return Ok(decision),
            }
        }
        Ok(ToolDecision::Allow)
    }

    async fn apply_after_tool_call(&self, tool: &ToolCall, result: &mut ToolResult) -> Result<()> {
        for hook in &self.hooks {
            hook.after_tool_call(tool, result).await?;
        }
        Ok(())
    }

    async fn apply_before_output_commit(&self, content: &mut Vec<OutputContent>) -> Result<()> {
        for hook in &self.hooks {
            hook.before_output_commit(content).await?;
        }
        Ok(())
    }

    async fn apply_after_output_commit(&self, content: &[OutputContent]) -> Result<()> {
        for hook in &self.hooks {
            hook.after_output_commit(content).await?;
        }
        Ok(())
    }
}

fn into_assistant_content(message: Message) -> Result<Vec<OutputContent>> {
    match message {
        Message::Assistant(content) => Ok(content),
        other => Err(Error::protocol(format!(
            "expected assistant message, got {other:?}"
        ))),
    }
}

fn normalize_for_history(message: Message) -> Message {
    match message {
        Message::Assistant(content) => Message::Assistant(
            content
                .into_iter()
                .filter(|c| !matches!(c, OutputContent::Thinking(_) | OutputContent::Image(_)))
                .collect(),
        ),
        other => other,
    }
}

enum TurnState<'a> {
    ModelRequest,
    ModelStreaming {
        stream: ModelStream<'a>,
        output_state: OutputStreamState,
    },
    ToolExecution {
        output_content: Vec<OutputContent>,
    },
    Finished,
}
