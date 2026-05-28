use copro_agent::{Agent, AgentEvent, AgentHook, ToolExecutionPolicy, ToolRouter, async_trait};
use copro_api::error::Result;
use copro_api::message::{
    InputContent, Message, OutputContent, ToolCall, ToolResult, ToolResultStatus,
};
use copro_api::request::GenerateRequest;
use copro_api::response::FinishReason;
use copro_api::stream::{Model, ModelStream, OutputContentDelta, OutputStreamEvent};
use copro_api::tool::ToolDefinition;
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

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
async fn before_output_commit_hook_can_modify_output() {
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
async fn before_output_delta_hook_can_modify_stream_and_history() {
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
    agent.hooks.push(Arc::new(DeltaRedactHook));

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
            AgentEvent::OutputDelta(OutputContentDelta::Text("redacted".to_string())),
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
async fn after_turn_hook_runs_before_final_event_is_yielded() {
    let completed = Arc::new(AtomicUsize::new(0));
    let mut agent = test_agent(vec![
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text("done".to_string()),
        },
        OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]);
    agent.hooks.push(Arc::new(TurnDoneHook {
        completed: Arc::clone(&completed),
    }));

    let mut stream = agent.run_stream();
    assert!(matches!(
        stream.next().await.transpose().unwrap(),
        Some(AgentEvent::OutputDelta(_))
    ));
    assert_eq!(completed.load(Ordering::SeqCst), 0);

    assert!(matches!(
        stream.next().await.transpose().unwrap(),
        Some(AgentEvent::OutputFinished { .. })
    ));
    assert_eq!(completed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn run_stream_stops_when_stop_signal_is_requested() {
    let completed = Arc::new(AtomicUsize::new(0));
    let mut agent = test_agent(vec![
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text("first".to_string()),
        },
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text(" second".to_string()),
        },
        OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]);
    agent.hooks.push(Arc::new(TurnDoneHook {
        completed: Arc::clone(&completed),
    }));
    let stop_signal = agent.stop_signal.clone();

    {
        let mut stream = agent.run_stream();
        assert_eq!(
            stream.next().await.transpose().unwrap(),
            Some(AgentEvent::OutputDelta(OutputContentDelta::Text(
                "first".to_string()
            )))
        );

        stop_signal.request_stop();
        assert!(stream.next().await.is_none());
    }

    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert!(agent.messages.is_empty());
}

#[tokio::test]
async fn run_stream_awaits_async_tool_router() {
    let mut agent = Agent::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );

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

#[tokio::test]
async fn run_stream_batches_parallel_tools_behind_serial_barriers() {
    let router = Arc::new(ConcurrentToolRouter::default());
    let tool_router: Arc<dyn ToolRouter> = router.clone();
    let mut agent = Agent::new(
        Arc::new(MultiToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        tool_router,
    );

    let events = agent
        .run_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    let tool_names = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult(result) => Some(result.name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(tool_names, vec!["p1", "p2", "serial", "p3", "p4"]);
    assert_eq!(router.max_parallel.load(Ordering::SeqCst), 2);
    assert_eq!(router.barrier_violations.load(Ordering::SeqCst), 0);
}

fn test_agent(events: Vec<OutputStreamEvent>) -> Agent {
    Agent::new(Arc::new(TestModel { events }), Arc::new(EmptyToolRouter))
}

struct RedactHook;

#[async_trait]
impl AgentHook for RedactHook {
    async fn before_output_commit(&self, content: &mut Vec<OutputContent>) -> Result<()> {
        *content = vec![OutputContent::Text("redacted".to_string())];
        Ok(())
    }
}

struct DeltaRedactHook;

#[async_trait]
impl AgentHook for DeltaRedactHook {
    async fn before_output_delta(
        &self,
        _content_index: usize,
        delta: &mut OutputContentDelta,
    ) -> Result<()> {
        *delta = OutputContentDelta::Text("redacted".to_string());
        Ok(())
    }
}

struct TurnDoneHook {
    completed: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentHook for TurnDoneHook {
    async fn after_turn(&self, _messages: &[Message]) -> Result<()> {
        self.completed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct EmptyToolRouter;

#[async_trait]
impl ToolRouter for EmptyToolRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        Ok(Vec::new())
    }

    async fn execute(&self, _call: ToolCall) -> Result<ToolResult> {
        unreachable!("empty runtime has no tools")
    }
}

struct DoubleToolRouter;

#[async_trait]
impl ToolRouter for DoubleToolRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        Ok(vec![ToolDefinition {
            name: "double".to_string(),
            description: "Double an integer.".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }])
    }

    async fn execute(&self, call: ToolCall) -> Result<ToolResult> {
        let ToolCall {
            id,
            name,
            arguments,
        } = call;
        tokio::task::yield_now().await;
        let result = match arguments.get("value").and_then(Value::as_i64) {
            Some(value) => ToolResult {
                call_id: id,
                name,
                status: ToolResultStatus::Success,
                content: vec![InputContent::Text((value * 2).to_string())],
            },
            None => ToolResult {
                call_id: id,
                name,
                status: ToolResultStatus::Error,
                content: vec![InputContent::Text("missing value".to_string())],
            },
        };

        Ok(result)
    }
}

#[derive(Default)]
struct ConcurrentToolRouter {
    active_parallel: AtomicUsize,
    active_serial: AtomicUsize,
    max_parallel: AtomicUsize,
    barrier_violations: AtomicUsize,
}

#[async_trait]
impl ToolRouter for ConcurrentToolRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        Ok(["p1", "p2", "serial", "p3", "p4"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.to_string(),
                description: format!("{name} test tool"),
                parameters: serde_json::json!({"type": "object"}),
            })
            .collect())
    }

    async fn execution_policy(&self, call: &ToolCall) -> Result<ToolExecutionPolicy> {
        Ok(if call.name == "serial" {
            ToolExecutionPolicy::Serial
        } else {
            ToolExecutionPolicy::Parallel
        })
    }

    async fn execute(&self, call: ToolCall) -> Result<ToolResult> {
        if call.name == "serial" {
            if self.active_parallel.load(Ordering::SeqCst) != 0 {
                self.barrier_violations.fetch_add(1, Ordering::SeqCst);
            }
            if self.active_serial.fetch_add(1, Ordering::SeqCst) != 0 {
                self.barrier_violations.fetch_add(1, Ordering::SeqCst);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.active_serial.fetch_sub(1, Ordering::SeqCst);
        } else {
            if self.active_serial.load(Ordering::SeqCst) != 0 {
                self.barrier_violations.fetch_add(1, Ordering::SeqCst);
            }
            let active = self.active_parallel.fetch_add(1, Ordering::SeqCst) + 1;
            update_max(&self.max_parallel, active);
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.active_parallel.fetch_sub(1, Ordering::SeqCst);
        }

        Ok(ToolResult {
            call_id: call.id,
            name: call.name,
            status: ToolResultStatus::Success,
            content: vec![InputContent::Text("ok".to_string())],
        })
    }
}

fn update_max(max: &AtomicUsize, value: usize) {
    let mut current = max.load(Ordering::SeqCst);
    while value > current {
        match max.compare_exchange(current, value, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(next) => current = next,
        }
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

struct MultiToolThenDoneModel {
    calls: AtomicUsize,
}

impl Model for MultiToolThenDoneModel {
    fn stream(&self, _request: GenerateRequest) -> ModelStream<'_> {
        let events = match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => vec![
                tool_call_delta(0, "call-p1", "p1"),
                tool_call_delta(1, "call-p2", "p2"),
                tool_call_delta(2, "call-serial", "serial"),
                tool_call_delta(3, "call-p3", "p3"),
                tool_call_delta(4, "call-p4", "p4"),
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

fn tool_call_delta(content_index: usize, id: &str, name: &str) -> OutputStreamEvent {
    OutputStreamEvent::Delta {
        content_index,
        delta: OutputContentDelta::ToolCall {
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            arguments: "{}".to_string(),
        },
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
