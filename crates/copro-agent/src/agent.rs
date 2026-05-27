use crate::event::{AgentEvent, AgentStream};
use crate::hook::{AgentHook, ToolDecision, ToolExecuteContext, ToolResultContext};
use copro_core::error::{Error, Result};
use copro_core::message::{InputContent, Message, OutputContent, ToolResultStatus};
use copro_core::provider::{Chat, ProviderRegistry};
use copro_core::request::{GenerateRequest, GenerateRequestOptions};
use copro_core::stream::{OutputStreamEvent, OutputStreamState};
use copro_core::tool::{ErasedTool, ToolDefinition};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use std::sync::Arc;

/// Conversational agent bound to one chat model with tools, hooks, and conversation state.
pub struct Agent {
    pub chat: Arc<dyn Chat>,
    pub tools: Vec<Arc<dyn ErasedTool>>,
    pub hooks: Vec<Arc<dyn AgentHook>>,
    pub max_tool_rounds: usize,
    pub messages: Vec<Message>,
}

impl Agent {
    pub fn new(chat: Arc<dyn Chat>) -> Self {
        Self {
            chat,
            tools: Vec::new(),
            hooks: Vec::new(),
            max_tool_rounds: 10,
            messages: Vec::new(),
        }
    }

    /// Convenience constructor that resolves a chat model from a registry.
    pub fn from_registry(registry: &ProviderRegistry, model_id: &str) -> Result<Self> {
        Ok(Self::new(registry.chat(model_id)?))
    }

    /// Run one streaming turn using this agent's bound chat model and conversation state.
    ///
    /// Model content is streamed as [`AgentEvent::OutputDelta`] events.
    /// Completed model outputs and tool results are yielded when they are
    /// committed to state. The final event is always [`AgentEvent::TurnFinish`].
    pub fn run_stream(&mut self) -> AgentStream<'_> {
        Box::pin(async_stream::try_stream! {
            let chat = Arc::clone(&self.chat);
            let mut done = false;

            for _round in 0..self.max_tool_rounds {
                let mut request = self.build_request(self.messages.clone());
                self.apply_before_request(&mut request)?;
                let mut stream = chat.stream(request);
                let mut output_state = OutputStreamState::new();

                loop {
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
                            yield AgentEvent::OutputDelta {
                                delta: delta.clone(),
                            };
                            output_state.apply(OutputStreamEvent::Delta {
                                content_index,
                                delta,
                            })?;
                        }
                        OutputStreamEvent::Finished { reason, usage } => {
                            let response = output_state
                                .apply(OutputStreamEvent::Finished { reason, usage })?
                                .ok_or_else(|| {
                                    Error::protocol("stream ended before finished event")
                                })?;

                            let finish_reason = response.finish_reason;
                            let usage = response.usage.clone();
                            let mut assistant_message = response.message;
                            self.apply_on_output_finished(&mut assistant_message)?;
                            let output_content = assistant_content(&assistant_message)?;
                            let has_tool_calls = output_content
                                .iter()
                                .any(|c| matches!(c, OutputContent::ToolCall { .. }));
                            self.messages.push(normalize_for_history(assistant_message));
                            yield AgentEvent::Output {
                                content: output_content.clone(),
                                finish_reason,
                                usage,
                            };

                            if !has_tool_calls {
                                done = true;
                                yield AgentEvent::TurnFinish;
                                break;
                            }

                            for item in &output_content {
                                if let OutputContent::ToolCall {
                                    id,
                                    name,
                                    arguments,
                                } = item
                                {
                                    let mut tool = ToolExecuteContext {
                                        call_id: id.clone(),
                                        name: name.clone(),
                                        arguments: arguments.clone(),
                                    };
                                    let result = match self.apply_before_tool_execute(&mut tool)? {
                                        ToolDecision::Allow => {
                                            self.execute_tool(&tool.name, &tool.arguments)
                                        }
                                        ToolDecision::Reject { reason } => ToolExecutionResult {
                                            status: ToolResultStatus::Error,
                                            content: vec![InputContent::Text { text: reason }],
                                        },
                                    };
                                    let mut result = ToolResultContext {
                                        call_id: tool.call_id,
                                        name: tool.name,
                                        status: result.status,
                                        content: result.content,
                                    };
                                    self.apply_after_tool_result(&mut result)?;
                                    let tool_message = Message::Tool {
                                        call_id: result.call_id.clone(),
                                        name: result.name.clone(),
                                        status: result.status.clone(),
                                        content: result.content.clone(),
                                    };
                                    let event = tool_result_event(&tool_message)?;
                                    self.messages.push(tool_message);
                                    yield event;
                                }
                            }

                            break;
                        }
                    }
                }

                if done {
                    break;
                }
            }

            if !done {
                Err(Error::client("max tool rounds exceeded"))?;
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

    fn apply_before_request(&self, request: &mut GenerateRequest) -> Result<()> {
        for hook in &self.hooks {
            hook.before_request(request)?;
        }
        Ok(())
    }

    fn apply_before_tool_execute(&self, tool: &mut ToolExecuteContext) -> Result<ToolDecision> {
        for hook in &self.hooks {
            match hook.before_tool_execute(tool)? {
                ToolDecision::Allow => {}
                decision => return Ok(decision),
            }
        }
        Ok(ToolDecision::Allow)
    }

    fn apply_after_tool_result(&self, result: &mut ToolResultContext) -> Result<()> {
        for hook in &self.hooks {
            hook.after_tool_result(result)?;
        }
        Ok(())
    }

    fn apply_on_output_finished(&self, message: &mut Message) -> Result<()> {
        for hook in &self.hooks {
            hook.on_output_finished(message)?;
        }
        Ok(())
    }

    fn execute_tool(&self, name: &str, arguments: &Map<String, Value>) -> ToolExecutionResult {
        let Some(tool) = self.tools.iter().find(|t| t.name() == name) else {
            return ToolExecutionResult {
                status: ToolResultStatus::Error,
                content: vec![InputContent::Text {
                    text: format!("unknown tool: {name}"),
                }],
            };
        };

        match tool.call_json(Value::Object(arguments.clone())) {
            Ok(output) => {
                let text = serde_json::to_string(&output).unwrap_or_else(|_| format!("{output:?}"));
                ToolExecutionResult {
                    status: ToolResultStatus::Success,
                    content: vec![InputContent::Text { text }],
                }
            }
            Err(error) => ToolExecutionResult {
                status: ToolResultStatus::Error,
                content: vec![InputContent::Text { text: error }],
            },
        }
    }
}

fn assistant_content(message: &Message) -> Result<Vec<OutputContent>> {
    match message {
        Message::Assistant { content } => Ok(content.clone()),
        other => Err(Error::protocol(format!(
            "expected assistant message, got {other:?}"
        ))),
    }
}

fn tool_result_event(message: &Message) -> Result<AgentEvent> {
    match message {
        Message::Tool {
            call_id,
            name,
            status,
            content,
        } => Ok(AgentEvent::ToolResult {
            call_id: call_id.clone(),
            name: name.clone(),
            status: status.clone(),
            content: content.clone(),
        }),
        other => Err(Error::protocol(format!(
            "expected tool message, got {other:?}"
        ))),
    }
}

fn normalize_for_history(message: Message) -> Message {
    match message {
        Message::Assistant { content } => Message::Assistant {
            content: content
                .into_iter()
                .filter(|c| {
                    !matches!(
                        c,
                        OutputContent::Thinking { .. } | OutputContent::Image { .. }
                    )
                })
                .collect(),
        },
        other => other,
    }
}

struct ToolExecutionResult {
    status: ToolResultStatus,
    content: Vec<InputContent>,
}
