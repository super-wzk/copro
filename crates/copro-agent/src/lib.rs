pub mod runtime;

use copro_core::error::{Error, Result};
use copro_core::message::{InputContent, Message, OutputContent, ToolResultStatus};
use copro_core::provider::ProviderRegistry;
use copro_core::request::{GenerateRequest, GenerateRequestOptions};
use copro_core::response::{FinishReason, Usage};
use copro_core::stream::{OutputContentDelta, OutputStreamEvent, OutputStreamState};
use copro_core::tool::{ErasedTool, ToolDefinition};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use std::pin::Pin;
use std::sync::Arc;

/// Events emitted during one agent turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// A streaming model output delta before the output is committed.
    OutputDelta { delta: OutputContentDelta },
    /// A complete model output committed as an assistant message.
    Output {
        content: Vec<OutputContent>,
        finish_reason: FinishReason,
        usage: Option<Usage>,
    },
    /// A tool execution result committed as a tool message.
    ToolResult {
        call_id: String,
        name: String,
        status: ToolResultStatus,
        content: Vec<InputContent>,
    },
    /// The whole agent turn has completed after all tool rounds.
    TurnFinish,
}

/// A stream of [`AgentEvent`]s produced by an agent turn.
pub type AgentStream<'a> =
    Pin<Box<dyn futures_util::Stream<Item = Result<AgentEvent>> + Send + 'a>>;

/// Conversational agent that holds providers, tools, and conversation state.
pub struct Agent {
    pub registry: ProviderRegistry,
    pub tools: Vec<Arc<dyn ErasedTool>>,
    pub max_tool_rounds: usize,
    pub messages: Vec<Message>,
}

impl Agent {
    pub fn new(registry: ProviderRegistry) -> Self {
        Self {
            registry,
            tools: Vec::new(),
            max_tool_rounds: 10,
            messages: Vec::new(),
        }
    }

    /// Run one streaming turn against `model_id` using this agent's conversation state.
    ///
    /// Model content is streamed as [`AgentEvent::OutputDelta`] events.
    /// Completed model outputs and tool results are yielded when they are
    /// committed to state. The final event is always [`AgentEvent::TurnFinish`].
    pub fn run_stream<'a>(&'a mut self, model_id: &'a str) -> AgentStream<'a> {
        Box::pin(async_stream::try_stream! {
            let chat = self.registry.chat(model_id)?;
            let mut done = false;

            for _round in 0..self.max_tool_rounds {
                let request = self.build_request(self.messages.clone());
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
                            let finish_reason = response.finish_reason;
                            let usage = response.usage.clone();
                            let assistant_message = strip_thinking(response.message);
                            let output_content = match &assistant_message {
                                Message::Assistant { content } => content.clone(),
                                other => {
                                    Err(Error::protocol(format!(
                                        "expected assistant message, got {other:?}"
                                    )))?;
                                    unreachable!()
                                }
                            };
                            self.messages.push(assistant_message);
                            yield AgentEvent::Output {
                                content: output_content,
                                finish_reason,
                                usage,
                            };

                            if !has_tool_calls {
                                done = true;
                                yield AgentEvent::TurnFinish;
                                break;
                            }

                            for item in &content {
                                if let OutputContent::ToolCall {
                                    id,
                                    name,
                                    arguments,
                                } = item
                                {
                                    let result = self.execute_tool(name, arguments);
                                    let tool_message = Message::Tool {
                                        call_id: id.clone(),
                                        name: name.clone(),
                                        status: result.status.clone(),
                                        content: result.content.clone(),
                                    };
                                    self.messages.push(tool_message);
                                    yield AgentEvent::ToolResult {
                                        call_id: id.clone(),
                                        name: name.clone(),
                                        status: result.status,
                                        content: result.content,
                                    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use copro_core::model::ModelDefinition;
    use copro_core::provider::{Chat, Provider};
    use copro_core::stream::ModelStream;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn run_stream_commits_assistant_message() {
        let mut registry = ProviderRegistry::new();
        registry.register_provider(TestProvider {
            events: vec![
                OutputStreamEvent::Delta {
                    content_index: 0,
                    delta: OutputContentDelta::Text {
                        text: "Hello".to_string(),
                    },
                },
                OutputStreamEvent::Finished {
                    reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        });
        registry.register_model(ModelDefinition::new("test", "test-model"));
        let mut agent = Agent::new(registry);
        agent.messages = vec![Message::User {
            content: vec![InputContent::Text {
                text: "hi".to_string(),
            }],
        }];

        let events = agent
            .run_stream("test-model")
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();

        let assistant = Message::Assistant {
            content: vec![OutputContent::Text {
                text: "Hello".to_string(),
            }],
        };
        assert_eq!(
            events,
            vec![
                AgentEvent::OutputDelta {
                    delta: OutputContentDelta::Text {
                        text: "Hello".to_string(),
                    },
                },
                AgentEvent::Output {
                    content: vec![OutputContent::Text {
                        text: "Hello".to_string(),
                    }],
                    finish_reason: FinishReason::Stop,
                    usage: None,
                },
                AgentEvent::TurnFinish,
            ]
        );
        assert_eq!(
            agent.messages,
            vec![
                Message::User {
                    content: vec![InputContent::Text {
                        text: "hi".to_string(),
                    }],
                },
                assistant,
            ]
        );
    }

    struct TestProvider {
        events: Vec<OutputStreamEvent>,
    }

    impl Provider for TestProvider {
        fn id(&self) -> &str {
            "test"
        }

        fn chat(&self, _id: &str, _config: Value) -> Result<Arc<dyn Chat>> {
            Ok(Arc::new(TestChat {
                events: self.events.clone(),
            }))
        }
    }

    struct TestChat {
        events: Vec<OutputStreamEvent>,
    }

    impl Chat for TestChat {
        fn stream(&self, _request: GenerateRequest) -> ModelStream<'_> {
            Box::pin(futures_util::stream::iter(
                self.events.clone().into_iter().map(Ok),
            ))
        }
    }
}
