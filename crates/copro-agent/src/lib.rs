pub mod runtime;

use copro_core::error::{Error, Result};
use copro_core::message::{InputContent, Message, OutputContent, ToolResultStatus};
use copro_core::provider::ProviderRegistry;
use copro_core::request::{GenerateRequest, GenerateRequestOptions};
use copro_core::response::FinishReason;
use copro_core::stream::{OutputContentDelta, OutputStreamEvent, OutputStreamState};
use copro_core::tool::{ErasedTool, ToolDefinition};
use futures_util::StreamExt;
pub use runtime::{RequestDeadline, RuntimeOptions};
use serde_json::{Map, Value};
use std::pin::Pin;
use std::sync::Arc;

/// Events emitted during one agent turn.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Text(String),
    Thinking(String),
    ToolCall {
        id: String,
        name: String,
        arguments: Map<String, Value>,
    },
    ToolOutput {
        name: String,
        result: String,
    },
    Finished {
        reason: FinishReason,
    },
}

/// A stream of [`AgentEvent`]s produced by an agent turn.
pub type AgentStream<'a> =
    Pin<Box<dyn futures_util::Stream<Item = Result<AgentEvent>> + Send + 'a>>;

/// Conversational agent that holds providers, tools, and runtime config.
pub struct Agent {
    registry: ProviderRegistry,
    tools: Vec<Arc<dyn ErasedTool>>,
    runtime: RuntimeOptions,
    max_tool_rounds: usize,
}

impl Agent {
    pub fn new(registry: ProviderRegistry) -> Self {
        Self {
            registry,
            tools: Vec::new(),
            runtime: RuntimeOptions::default(),
            max_tool_rounds: 10,
        }
    }

    pub fn with_tools(mut self, tools: impl IntoIterator<Item = Arc<dyn ErasedTool>>) -> Self {
        self.tools.extend(tools);
        self
    }

    pub fn with_runtime(mut self, options: RuntimeOptions) -> Self {
        self.runtime = options;
        self
    }

    pub fn with_max_tool_rounds(mut self, max: usize) -> Self {
        self.max_tool_rounds = max;
        self
    }

    /// Run one turn against `model_id` with the given `messages`.
    ///
    /// Returns all events emitted during the turn, collected into a vector.
    /// For streaming consumption, use [`Agent::run_stream`].
    pub async fn run(&self, model_id: &str, messages: Vec<Message>) -> Result<Vec<AgentEvent>> {
        let mut stream = self.run_stream(model_id, messages);
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event?);
        }
        Ok(events)
    }

    /// Run one turn against `model_id` with the given `messages`, yielding
    /// events as they arrive.
    ///
    /// Text and thinking content is streamed token-by-token. Tool calls are
    /// yielded as complete events once the model finishes generating them.
    /// Tool outputs are yielded after each tool executes. The final event is
    /// always [`AgentEvent::Finished`].
    ///
    /// Tool calls are executed automatically and their results fed back to the
    /// model; this repeats until the model produces a non-tool response or
    /// `max_tool_rounds` is reached.
    pub fn run_stream<'a>(&'a self, model_id: &'a str, messages: Vec<Message>) -> AgentStream<'a> {
        Box::pin(async_stream::try_stream! {
            let mut messages = messages;
            let chat = self.registry.chat(model_id)?;
            let deadline = RequestDeadline::from_options(&self.runtime);
            let mut done = false;

            for _round in 0..self.max_tool_rounds {
                let request = self.build_request(messages.clone());
                let mut stream = chat.stream(request);
                let mut state = OutputStreamState::new();

                loop {
                    let event = deadline
                        .next_model_event(&mut stream)
                        .await?
                        .ok_or_else(|| {
                            Error::protocol("stream ended before finished event")
                        })?;

                    // Yield text / thinking deltas immediately so consumers
                    // see output word-by-word.  We extract the event payload
                    // before the yield so no borrows cross the suspend point.
                    let maybe_yield = match &event {
                        OutputStreamEvent::Delta { delta, .. } => match delta {
                            OutputContentDelta::Text { text } => {
                                Some(AgentEvent::Text(text.clone()))
                            }
                            OutputContentDelta::Thinking { text } => {
                                Some(AgentEvent::Thinking(text.clone()))
                            }
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(e) = maybe_yield {
                        yield e;
                    }

                    // Feed every event into the accumulator so we can
                    // reconstruct tool calls and the final message.
                    match event {
                        OutputStreamEvent::Delta {
                            content_index,
                            delta,
                        } => {
                            state.apply(OutputStreamEvent::Delta {
                                content_index,
                                delta,
                            })?;
                        }
                        OutputStreamEvent::Finished { reason, usage } => {
                            let response = state
                                .apply(OutputStreamEvent::Finished {
                                    reason,
                                    usage,
                                })?
                                .ok_or_else(|| {
                                    Error::protocol(
                                        "stream ended before finished event",
                                    )
                                })?;

                            let content = match &response.message {
                                Message::Assistant { content } => content.clone(),
                                other => {
                                    Err(Error::protocol(format!(
                                        "expected assistant message, got {other:?}"
                                    )))?;
                                    unreachable!()
                                }
                            };

                            let has_tool_calls = content
                                .iter()
                                .any(|c| matches!(c, OutputContent::ToolCall { .. }));

                            // Emit complete tool calls (accumulated during
                            // streaming).  Tool call arguments are reassembled
                            // from deltas by OutputStreamState.
                            for item in &content {
                                if let OutputContent::ToolCall {
                                    id,
                                    name,
                                    arguments,
                                } = item
                                {
                                    yield AgentEvent::ToolCall {
                                        id: id.clone(),
                                        name: name.clone(),
                                        arguments: arguments.clone(),
                                    };
                                }
                            }

                            if !has_tool_calls {
                                yield AgentEvent::Finished {
                                    reason: response.finish_reason,
                                };
                                done = true;
                                break;
                            }

                            // Execute tools and feed results back into the
                            // conversation.
                            messages.push(strip_thinking(response.message));
                            for item in &content {
                                if let OutputContent::ToolCall {
                                    id,
                                    name,
                                    arguments,
                                } = item
                                {
                                    let result =
                                        self.execute_tool(name, arguments);
                                    let result_text = result
                                        .content
                                        .iter()
                                        .filter_map(|c| {
                                            if let InputContent::Text {
                                                text,
                                            } = c
                                            {
                                                Some(text.clone())
                                            } else {
                                                None
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n");

                                    yield AgentEvent::ToolOutput {
                                        name: name.clone(),
                                        result: result_text.clone(),
                                    };

                                    messages.push(Message::Tool {
                                        call_id: id.clone(),
                                        name: name.clone(),
                                        status: result.status,
                                        content: result.content,
                                    });
                                }
                            }

                            break; // continue to next tool round
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

fn strip_thinking(message: Message) -> Message {
    match message {
        Message::Assistant { content } => Message::Assistant {
            content: content
                .into_iter()
                .filter(|c| !matches!(c, OutputContent::Thinking { .. }))
                .collect(),
        },
        other => other,
    }
}

struct ToolExecutionResult {
    status: ToolResultStatus,
    content: Vec<InputContent>,
}
