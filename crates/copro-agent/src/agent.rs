use crate::event::{AgentEvent, AgentStream};
use crate::hook::{AgentHook, ToolDecision};
use copro_core::error::{Error, Result};
use copro_core::message::{
    InputContent, Message, OutputContent, ToolCall, ToolResult, ToolResultStatus,
};
use copro_core::request::{GenerateRequest, GenerateRequestOptions};
use copro_core::stream::{Model, ModelStream, OutputStreamEvent, OutputStreamState};
use copro_core::tool::{ErasedTool, ToolDefinition};
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;

/// Conversational agent bound to one model with tools, hooks, and conversation state.
pub struct Agent {
    pub model: Arc<dyn Model>,
    pub tools: Vec<Arc<dyn ErasedTool>>,
    pub hooks: Vec<Arc<dyn AgentHook>>,
    pub max_tool_rounds: usize,
    pub messages: Vec<Message>,
}

impl Agent {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            model,
            tools: Vec::new(),
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

                        let mut request = self.build_request(self.messages.clone());
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
                                let mut assistant_message = response.message;
                                self.apply_on_output_finished(&mut assistant_message).await?;
                                let output_content = assistant_content(&assistant_message)?;
                                self.messages.push(normalize_for_history(assistant_message));
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
                            let mut result = match self.apply_before_tool_execute(&mut tool).await? {
                                ToolDecision::Allow => {
                                    self.execute_tool(&tool).await
                                }
                                ToolDecision::Reject { reason } => ToolResult {
                                    call_id: tool.id,
                                    name: tool.name,
                                    status: ToolResultStatus::Error,
                                    content: vec![InputContent::Text(reason)],
                                },
                            };
                            self.apply_after_tool_result(&mut result).await?;
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

    fn build_request(&self, messages: Vec<Message>) -> GenerateRequest {
        let tool_defs: Vec<ToolDefinition> =
            self.tools.iter().map(|tool| tool.as_ref().into()).collect();

        GenerateRequest {
            messages,
            tools: tool_defs,
            hosted_tools: Vec::new(),
            tool_choice: None,
            options: GenerateRequestOptions::default(),
        }
    }

    async fn apply_before_request(&self, request: &mut GenerateRequest) -> Result<()> {
        for hook in &self.hooks {
            hook.before_request(request).await?;
        }
        Ok(())
    }

    async fn apply_before_tool_execute(&self, tool: &mut ToolCall) -> Result<ToolDecision> {
        for hook in &self.hooks {
            match hook.before_tool_execute(tool).await? {
                ToolDecision::Allow => {}
                decision => return Ok(decision),
            }
        }
        Ok(ToolDecision::Allow)
    }

    async fn apply_after_tool_result(&self, result: &mut ToolResult) -> Result<()> {
        for hook in &self.hooks {
            hook.after_tool_result(result).await?;
        }
        Ok(())
    }

    async fn apply_on_output_finished(&self, message: &mut Message) -> Result<()> {
        for hook in &self.hooks {
            hook.on_output_finished(message).await?;
        }
        Ok(())
    }

    async fn execute_tool(&self, call: &ToolCall) -> ToolResult {
        let Some(tool) = self.tools.iter().find(|t| t.name() == call.name) else {
            return ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                status: ToolResultStatus::Error,
                content: vec![InputContent::Text(format!("unknown tool: {}", call.name))],
            };
        };

        match tool.call_json(Value::Object(call.arguments.clone())).await {
            Ok(output) => {
                let text = serde_json::to_string(&output).unwrap_or_else(|_| format!("{output:?}"));
                ToolResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    status: ToolResultStatus::Success,
                    content: vec![InputContent::Text(text)],
                }
            }
            Err(error) => ToolResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                status: ToolResultStatus::Error,
                content: vec![InputContent::Text(error)],
            },
        }
    }
}

fn assistant_content(message: &Message) -> Result<Vec<OutputContent>> {
    match message {
        Message::Assistant(content) => Ok(content.clone()),
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
