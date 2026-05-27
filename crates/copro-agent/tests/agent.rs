use copro_agent::{Agent, AgentEvent, AgentHook, async_trait};
use copro_core::error::Result;
use copro_core::message::{InputContent, Message, OutputContent, ToolResult, ToolResultStatus};
use copro_core::request::GenerateRequest;
use copro_core::response::FinishReason;
use copro_core::stream::{Model, ModelStream, OutputContentDelta, OutputStreamEvent};
use copro_core::tool::ErasedTool;
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn run_stream_commits_assistant_message() {
    let mut agent = test_agent(vec![
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text("Hello".to_string()),
        },
        OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]);
    agent.messages = vec![Message::User(vec![InputContent::Text("hi".to_string())])];

    let events = agent
        .run_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    let assistant = Message::Assistant(vec![OutputContent::Text("Hello".to_string())]);
    assert_eq!(
        events,
        vec![
            AgentEvent::OutputDelta(OutputContentDelta::Text("Hello".to_string())),
            AgentEvent::OutputFinished {
                content: vec![OutputContent::Text("Hello".to_string())],
                reason: FinishReason::Stop,
                usage: None,
            },
        ]
    );
    assert_eq!(
        agent.messages,
        vec![
            Message::User(vec![InputContent::Text("hi".to_string())]),
            assistant,
        ]
    );
}

#[tokio::test]
async fn on_output_finished_hook_can_modify_output() {
    let mut agent = test_agent(vec![
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text("secret".to_string()),
        },
        OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]);
    agent.hooks.push(Arc::new(RedactHook));

    let events = agent
        .run_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    let redacted = vec![OutputContent::Text("redacted".to_string())];
    assert_eq!(
        events,
        vec![
            AgentEvent::OutputDelta(OutputContentDelta::Text("secret".to_string())),
            AgentEvent::OutputFinished {
                content: redacted.clone(),
                reason: FinishReason::Stop,
                usage: None,
            },
        ]
    );
    assert_eq!(agent.messages, vec![Message::Assistant(redacted)]);
}

#[tokio::test]
async fn run_stream_awaits_async_erased_tool() {
    let mut agent = Agent::new(Arc::new(ToolThenDoneModel {
        calls: AtomicUsize::new(0),
    }));
    agent.tools.push(Arc::new(AsyncDoubleTool));

    let events = agent
        .run_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResult(ToolResult {
            name,
            status: ToolResultStatus::Success,
            content,
            ..
        }) if name == "double"
            && content == &vec![InputContent::Text("42".to_string())]
    )));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::OutputFinished {
            content,
            reason: FinishReason::Stop,
            usage: None,
        }) if content == &vec![OutputContent::Text("done".to_string())]
    ));
}

fn test_agent(events: Vec<OutputStreamEvent>) -> Agent {
    Agent::new(Arc::new(TestModel { events }))
}

struct RedactHook;

#[async_trait]
impl AgentHook for RedactHook {
    async fn on_output_finished(&self, content: &mut Vec<OutputContent>) -> Result<()> {
        *content = vec![OutputContent::Text("redacted".to_string())];
        Ok(())
    }
}

struct AsyncDoubleTool;

#[async_trait]
impl ErasedTool for AsyncDoubleTool {
    fn name(&self) -> &str {
        "double"
    }

    fn description(&self) -> &str {
        "Double an integer."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object"})
    }

    async fn call_json(&self, args: Value) -> std::result::Result<Value, String> {
        tokio::task::yield_now().await;
        let value = args
            .get("value")
            .and_then(Value::as_i64)
            .ok_or_else(|| "missing value".to_string())?;
        Ok(Value::from(value * 2))
    }
}

struct ToolThenDoneModel {
    calls: AtomicUsize,
}

impl Model for ToolThenDoneModel {
    fn stream(&self, _request: GenerateRequest) -> ModelStream<'_> {
        let events = match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => vec![
                OutputStreamEvent::Delta {
                    content_index: 0,
                    delta: OutputContentDelta::ToolCall {
                        id: Some("call-1".to_string()),
                        name: Some("double".to_string()),
                        arguments: r#"{"value":21}"#.to_string(),
                    },
                },
                OutputStreamEvent::Finished {
                    reason: FinishReason::ToolCalls,
                    usage: None,
                },
            ],
            _ => vec![
                OutputStreamEvent::Delta {
                    content_index: 0,
                    delta: OutputContentDelta::Text("done".to_string()),
                },
                OutputStreamEvent::Finished {
                    reason: FinishReason::Stop,
                    usage: None,
                },
            ],
        };
        Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
    }
}

struct TestModel {
    events: Vec<OutputStreamEvent>,
}

impl Model for TestModel {
    fn stream(&self, _request: GenerateRequest) -> ModelStream<'_> {
        Box::pin(futures_util::stream::iter(
            self.events.clone().into_iter().map(Ok),
        ))
    }
}
