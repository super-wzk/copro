use copro_agent::{
    AgentAction, AgentCheckpoint, AgentControl, AgentEvent, AgentHistory, AgentInterruptReason,
    AgentOutcome, AgentTurnConfig, AgentTurnHandle, AgentTurnState, CancellationToken,
    InputMessage, OutputMessage, ToolExecutionPolicy, ToolResultReplacement, ToolRouter,
    async_trait, start_turn,
};
use copro_api::error::Result;
use copro_api::message::{
    InputContent, Message, OutputContent, ToolCall, ToolResult, ToolResultStatus,
};
use copro_api::request::{GenerateRequest, GenerateRequestOptions};
use copro_api::response::FinishReason;
use copro_api::stream::{Model, ModelStream, OutputContentDelta, OutputStreamEvent};
use copro_api::tool::{HostedToolSpec, ToolChoice, ToolDefinition};
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

#[derive(Debug, Clone, PartialEq)]
enum StreamEvent {
    ModelDelta(OutputContentDelta),
    AssistantCommitted {
        content: Vec<OutputContent>,
        reason: FinishReason,
        usage: Option<copro_api::response::Usage>,
    },
    ToolStarted(ToolCall),
    ToolResultCommitted(ToolResult),
}

fn stream_events(events: &[AgentEvent]) -> Vec<StreamEvent> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ModelDelta { delta, .. } => Some(StreamEvent::ModelDelta(delta.clone())),
            AgentEvent::AssistantCommitted {
                content,
                reason,
                usage,
                ..
            } => Some(StreamEvent::AssistantCommitted {
                content: content.clone(),
                reason: *reason,
                usage: usage.clone(),
            }),
            AgentEvent::ToolStarted { tool, .. } => Some(StreamEvent::ToolStarted(tool.clone())),
            AgentEvent::ToolResultCommitted { result, .. } => {
                Some(StreamEvent::ToolResultCommitted(result.clone()))
            }
            _ => None,
        })
        .collect()
}

#[derive(Clone)]
struct TestSession {
    history: Arc<AsyncMutex<AgentHistory>>,
    config: Arc<AsyncMutex<AgentTurnConfig>>,
    last_turn: Arc<AsyncMutex<Option<AgentTurnHandle>>>,
    model: Arc<dyn Model>,
    tools: Arc<dyn ToolRouter>,
}

impl TestSession {
    fn new(model: Arc<dyn Model>, tools: Arc<dyn ToolRouter>) -> Self {
        Self::from_history(AgentHistory::default(), model, tools)
    }

    fn from_history(
        history: AgentHistory,
        model: Arc<dyn Model>,
        tools: Arc<dyn ToolRouter>,
    ) -> Self {
        Self::from_parts(history, AgentTurnConfig::default(), model, tools)
    }

    fn from_parts(
        history: AgentHistory,
        config: AgentTurnConfig,
        model: Arc<dyn Model>,
        tools: Arc<dyn ToolRouter>,
    ) -> Self {
        Self {
            history: Arc::new(AsyncMutex::new(history)),
            config: Arc::new(AsyncMutex::new(config)),
            last_turn: Arc::new(AsyncMutex::new(None)),
            model,
            tools,
        }
    }

    async fn start_turn(&self) -> Result<AgentTurnHandle> {
        self.sync_history().await;
        let history = self.history.lock().await.clone();
        let config = self.config.lock().await.clone();
        let turn = start_turn(
            history,
            config,
            Arc::clone(&self.model),
            Arc::clone(&self.tools),
        );
        *self.last_turn.lock().await = Some(turn.clone());
        Ok(turn)
    }

    async fn push_input(&self, message: InputMessage) -> Result<()> {
        self.sync_history().await;
        self.history.lock().await.push_input(message);
        Ok(())
    }

    async fn replace_messages(&self, messages: Vec<Message>) -> Result<()> {
        self.sync_history().await;
        *self.history.lock().await = AgentHistory::from_messages(messages);
        Ok(())
    }

    async fn messages(&self) -> Result<Vec<Message>> {
        self.sync_history().await;
        Ok(self.history.lock().await.messages().to_vec())
    }

    async fn history(&self) -> Result<AgentHistory> {
        self.sync_history().await;
        Ok(self.history.lock().await.clone())
    }

    async fn config(&self) -> Result<AgentTurnConfig> {
        Ok(self.config.lock().await.clone())
    }

    async fn set_options(&self, options: GenerateRequestOptions) -> Result<()> {
        let mut config = self.config.lock().await;
        *config = config.clone().with_options(options);
        Ok(())
    }

    async fn set_tool_choice(&self, tool_choice: Option<ToolChoice>) -> Result<()> {
        let mut config = self.config.lock().await;
        *config = config.clone().with_tool_choice(tool_choice);
        Ok(())
    }

    async fn set_hosted_tools(&self, hosted_tools: Vec<HostedToolSpec>) -> Result<()> {
        let mut config = self.config.lock().await;
        *config = config.clone().with_hosted_tools(hosted_tools);
        Ok(())
    }

    async fn sync_history(&self) {
        let last_turn = self.last_turn.lock().await.clone();
        if let Some(turn) = last_turn {
            let history = turn.into_history().await;
            *self.history.lock().await = history;
            *self.last_turn.lock().await = None;
        }
    }
}

async fn collect_agent_events(agent: &TestSession) -> Vec<AgentEvent> {
    let run = agent.start_turn().await.unwrap();
    run.events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap()
}

#[tokio::test]
async fn turn_events_commit_assistant_message() {
    let agent = test_agent(vec![
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text("Hello".to_string()),
        },
        OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]);
    agent
        .replace_messages(vec![Message::user(vec![InputContent::Text(
            "hi".to_string(),
        )])])
        .await
        .unwrap();

    let events = collect_agent_events(&agent).await;

    let assistant = Message::assistant(vec![OutputContent::Text("Hello".to_string())]);
    assert_eq!(
        stream_events(&events),
        vec![
            StreamEvent::ModelDelta(OutputContentDelta::Text("Hello".to_string())),
            StreamEvent::AssistantCommitted {
                content: vec![OutputContent::Text("Hello".to_string())],
                reason: FinishReason::Stop,
                usage: None,
            },
        ]
    );
    assert_eq!(
        agent.messages().await.unwrap(),
        vec![
            Message::user(vec![InputContent::Text("hi".to_string())]),
            assistant,
        ]
    );
}

#[tokio::test]
async fn turn_handle_abort_turn_at_model_delta_does_not_commit_assistant_message() {
    let agent = test_agent(vec![
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
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(
            report.pending_outcome(),
            AgentOutcome::ModelDelta {
                delta: OutputContentDelta::Text(text),
                ..
            } if text == "first"
        ) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };

    report.control(AgentControl::AbortTurn).await.unwrap();
    let remaining = run
        .clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert!(stream_events(&remaining).is_empty());
    assert!(
        remaining
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnAborted))
    );

    assert!(agent.messages().await.unwrap().is_empty());
}

#[tokio::test]
async fn abort_turn_does_not_cancel_later_runs() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let first = agent.start_turn().await.unwrap();
    let report = first.step_until_control().await.unwrap();

    report.control(AgentControl::AbortTurn).await.unwrap();
    let first_events = first
        .clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert!(
        first_events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnAborted))
    );

    let second_events = collect_agent_events(&agent).await;
    assert!(
        second_events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFinished))
    );
    assert!(
        !second_events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnAborted))
    );
}

#[tokio::test]
async fn turn_events_expose_ready_state_before_step_starts() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();
    let mut stream = run.clone().events();

    loop {
        let event = stream.next().await.transpose().unwrap().unwrap();
        if let AgentEvent::StepReady { step } = event {
            assert_eq!(step.action, AgentAction::LoadTools);
            assert_eq!(
                run.state().await.unwrap(),
                AgentTurnState::Ready {
                    next: AgentAction::LoadTools,
                    step_id: step.id,
                }
            );
            break;
        }
    }
}

#[tokio::test]
async fn turn_events_await_async_tool_router() {
    let agent = TestSession::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );

    let events = collect_agent_events(&agent).await;

    let stream_events = stream_events(&events);
    let started_index = stream_events
        .iter()
        .position(|event| {
            matches!(
                event,
                StreamEvent::ToolStarted(ToolCall { id, name, .. })
                    if id.as_str() == "call-1" && name == "double"
            )
        })
        .unwrap();
    let result_index = stream_events
        .iter()
        .position(|event| {
            matches!(
                event,
                StreamEvent::ToolResultCommitted(ToolResult {
                    name,
                    status: ToolResultStatus::Success,
                    content,
                    ..
                }) if name == "double"
                    && content == &vec![InputContent::Text("42".to_string())]
            )
        })
        .unwrap();
    assert!(started_index < result_index);
    assert!(matches!(
        stream_events.last(),
        Some(StreamEvent::AssistantCommitted {
            content,
            reason: FinishReason::Stop,
            usage: None,
        }) if content == &vec![OutputContent::Text("done".to_string())]
    ));
}

#[tokio::test]
async fn tool_task_panic_is_committed_as_error_tool_result() {
    let agent = TestSession::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PanicToolRouter),
    );

    let events = collect_agent_events(&agent).await;

    let stream_events = stream_events(&events);
    assert!(stream_events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolResultCommitted(ToolResult {
            call_id,
            name,
            status: ToolResultStatus::Error,
            content,
        }) if call_id.as_str() == "call-1"
            && name == "double"
            && content == &vec![InputContent::Text("tool task panicked: boom".to_string())]
    )));
    assert!(matches!(
        stream_events.last(),
        Some(StreamEvent::AssistantCommitted {
            content,
            reason: FinishReason::Stop,
            usage: None,
        }) if content == &vec![OutputContent::Text("done".to_string())]
    ));
    assert!(matches!(
        agent.messages().await.unwrap().as_slice(),
        [
            Message::Output(OutputMessage::Assistant(assistant_content)),
            Message::Output(OutputMessage::Tool(ToolResult {
                call_id,
                name,
                status: ToolResultStatus::Error,
                content,
            })),
            Message::Output(OutputMessage::Assistant(done_content)),
        ] if matches!(assistant_content.as_slice(), [OutputContent::ToolCall(ToolCall { id, name, .. })]
            if id.as_str() == "call-1" && name == "double")
            && call_id.as_str() == "call-1"
            && name == "double"
            && content == &vec![InputContent::Text("tool task panicked: boom".to_string())]
            && done_content == &vec![OutputContent::Text("done".to_string())]
    ));
}

#[tokio::test]
async fn turn_events_batch_parallel_tools_behind_serial_barriers() {
    let router = Arc::new(ConcurrentToolRouter::default());
    let tool_router: Arc<dyn ToolRouter> = router.clone();
    let agent = TestSession::new(
        Arc::new(MultiToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        tool_router,
    );

    let events = collect_agent_events(&agent).await;

    let stream_events = stream_events(&events);
    let started_names = stream_events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolStarted(tool) => Some(tool.name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let tool_names = stream_events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolResultCommitted(result) => Some(result.name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(started_names, vec!["p1", "p2", "serial", "p3", "p4"]);
    assert_eq!(tool_names, vec!["p1", "p2", "serial", "p3", "p4"]);
    assert_eq!(router.max_parallel.load(Ordering::SeqCst), 2);
    assert_eq!(router.barrier_violations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn start_turn_steps_to_model_delta_boundary() {
    let agent = test_agent(vec![
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text("Hello".to_string()),
        },
        OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]);
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(report.pending_outcome(), AgentOutcome::ModelDelta { .. }) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };

    assert!(matches!(report.step.action, AgentAction::ReadModelStream));
    assert_eq!(report.step().id.tick, 3);
    assert!(matches!(
        report.pending_outcome(),
        AgentOutcome::ModelDelta {
            content_index: 0,
            delta: OutputContentDelta::Text(text),
        } if text == "Hello"
    ));
    assert!(matches!(
        run.state().await.unwrap(),
        AgentTurnState::WaitingControl { .. }
    ));
    report.continue_turn().await.unwrap();
    let next = run.step_until_control().await.unwrap();
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::ModelDelta {
            delta: OutputContentDelta::Text(text),
            ..
        } if text == "Hello"
    )));
}

#[tokio::test]
async fn turn_control_point_drop_continues_boundary() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();

    let first = run.step_until_control().await.unwrap();
    let first_step_id = first.step().id;
    assert!(matches!(
        first.pending_outcome(),
        AgentOutcome::ToolsLoaded(_)
    ));
    drop(first);

    let second = run.step_until_control().await.unwrap();
    assert!(matches!(
        second.pending_outcome(),
        AgentOutcome::RequestBuilt(_)
    ));
    assert!(second.events().iter().any(|event| matches!(
        event,
        AgentEvent::StepCompleted {
            step,
            outcome: AgentOutcome::ToolsLoaded(_),
        } if step.id == first_step_id
    )));
}

#[tokio::test]
async fn turn_handle_pause_and_resume_at_boundary() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();

    let first = run.step_until_control().await.unwrap();
    let first_step_id = first.step().id;
    run.pause().await.unwrap();
    assert_eq!(
        run.state().await.unwrap(),
        AgentTurnState::Paused { at: first_step_id }
    );
    drop(first);

    run.resume().await.unwrap();
    let second = run.step_until_control().await.unwrap();
    assert!(second.events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnPaused { at, .. } if *at == first_step_id
    )));
    assert!(second.events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnResumed { at, .. } if *at == first_step_id
    )));
    assert_eq!(second.step().id.tick, first_step_id.tick + 1);
}

#[tokio::test]
async fn turn_handle_control_pause_emits_pause_and_resume_events() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();

    let first = run.step_until_control().await.unwrap();
    let first_step_id = first.step().id;
    first.control(AgentControl::Pause).await.unwrap();
    assert_eq!(
        run.state().await.unwrap(),
        AgentTurnState::Paused { at: first_step_id }
    );

    run.resume().await.unwrap();
    let second = run.step_until_control().await.unwrap();
    assert!(second.events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnPaused { at, .. } if *at == first_step_id
    )));
    assert!(second.events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnResumed { at, .. } if *at == first_step_id
    )));
    assert_eq!(second.step().id.tick, first_step_id.tick + 1);
}

#[tokio::test]
async fn turn_handle_pause_request_waits_for_next_boundary() {
    let agent = TestSession::new(Arc::new(DelayedSecondDeltaModel), Arc::new(EmptyToolRouter));
    let run = agent.start_turn().await.unwrap();

    let first = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(
            report.pending_outcome(),
            AgentOutcome::ModelDelta {
                delta: OutputContentDelta::Text(text),
                ..
            } if text == "first"
        ) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };
    first.continue_turn().await.unwrap();

    let step_task = tokio::spawn({
        let run = run.clone();
        async move { run.step_until_control().await }
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    run.pause().await.unwrap();

    let paused = step_task.await.unwrap().unwrap();
    let paused_step_id = paused.step().id;
    assert!(matches!(
        paused.pending_outcome(),
        AgentOutcome::ModelDelta {
            delta: OutputContentDelta::Text(text),
            ..
        } if text == "second"
    ));
    assert_eq!(paused.state, AgentTurnState::Paused { at: paused_step_id });
    drop(paused);

    run.resume().await.unwrap();
    let next = run.step_until_control().await.unwrap();
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnPaused { at, .. } if *at == paused_step_id
    )));
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnResumed { at, .. } if *at == paused_step_id
    )));
}

#[tokio::test]
async fn turn_handle_finish_turn_finishes_without_turn_aborted() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();
    let report = run.step_until_control().await.unwrap();

    report.control(AgentControl::FinishTurn).await.unwrap();
    let events = run
        .clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::StepCompleted {
            outcome: AgentOutcome::ToolsLoaded(_),
            ..
        }
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnFinished))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnAborted))
    );
}

#[tokio::test]
async fn turn_handle_abort_turn_emits_turn_aborted() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();
    let report = run.step_until_control().await.unwrap();

    report.control(AgentControl::AbortTurn).await.unwrap();
    let events = run
        .clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnAborted))
    );
}

#[tokio::test]
async fn abort_turn_after_assistant_tool_call_recovers_tool_result() {
    let agent = TestSession::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(
            report.pending_outcome(),
            AgentOutcome::AssistantCommitted {
                reason: FinishReason::ToolCalls,
                ..
            }
        ) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };

    let report_step_id = report.step().id;
    report.control(AgentControl::AbortTurn).await.unwrap();
    assert_eq!(
        run.state().await.unwrap(),
        AgentTurnState::Recovering {
            after: report_step_id,
        }
    );

    let events = run
        .clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnRecovering { after, .. } if *after == report_step_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResultCommitted {
            result:
                ToolResult {
                    name,
                    status: ToolResultStatus::Error,
                    content,
                    ..
                },
            ..
        } if name == "double"
            && content == &vec![InputContent::Text("aborted by user".to_string())]
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnAborted))
    );
    assert!(matches!(
        agent.messages().await.unwrap().as_slice(),
        [
            Message::Output(OutputMessage::Assistant(content)),
            Message::Output(OutputMessage::Tool(ToolResult {
                call_id,
                name,
                status: ToolResultStatus::Error,
                content: result_content,
            }))
        ] if matches!(content.as_slice(), [OutputContent::ToolCall(ToolCall { id, name, .. })]
            if id.as_str() == "call-1" && name == "double")
            && call_id.as_str() == "call-1"
            && name == "double"
            && result_content == &vec![InputContent::Text("aborted by user".to_string())]
    ));
}

#[tokio::test]
async fn abort_turn_at_tool_started_recovers_tool_result() {
    let agent = TestSession::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(report.pending_outcome(), AgentOutcome::ToolStarted { .. }) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };

    let report_step_id = report.step().id;
    report.control(AgentControl::AbortTurn).await.unwrap();
    let events = run
        .clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnRecovering { after, .. } if *after == report_step_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResultCommitted {
            result:
                ToolResult {
                    name,
                    status: ToolResultStatus::Error,
                    content,
                    ..
                },
            ..
        } if name == "double"
            && content == &vec![InputContent::Text("aborted by user".to_string())]
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnAborted))
    );
}

#[tokio::test]
async fn turn_handle_preempt_inflight_model_stream_emits_turn_preempted() {
    let agent = TestSession::new(
        Arc::new(PendingAfterFirstDeltaModel),
        Arc::new(EmptyToolRouter),
    );
    let run = agent.start_turn().await.unwrap();

    let first = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(
            report.pending_outcome(),
            AgentOutcome::ModelDelta {
                delta: OutputContentDelta::Text(text),
                ..
            } if text == "first"
        ) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };
    first.continue_turn().await.unwrap();

    let step_task = tokio::spawn({
        let run = run.clone();
        async move { run.step_until_control().await }
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    run.preempt().await.unwrap();

    let interrupted = step_task.await.unwrap().unwrap();
    assert!(matches!(
        interrupted.pending_outcome(),
        AgentOutcome::ActionInterrupted {
            reason: AgentInterruptReason::Stopped
        }
    ));
    drop(interrupted);
    let events = run
        .clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::StepCompleted {
            outcome: AgentOutcome::ActionInterrupted {
                reason: AgentInterruptReason::Preempted
            },
            ..
        }
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnPreempted { .. }))
    );
}

#[tokio::test]
async fn turn_handle_preempt_inflight_tool_execution_recovers_tool_result() {
    let agent = TestSession::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PreemptibleToolRouter),
    );
    let run = agent.start_turn().await.unwrap();

    let started = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(report.pending_outcome(), AgentOutcome::ToolStarted { .. }) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };
    started.continue_turn().await.unwrap();

    let step_task = tokio::spawn({
        let run = run.clone();
        async move { run.step_until_control().await }
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    run.preempt().await.unwrap();

    let finished = step_task.await.unwrap().unwrap();
    assert!(matches!(
        finished.pending_outcome(),
        AgentOutcome::ToolFinished { .. }
    ));
    drop(finished);
    let events = run
        .clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnRecovering { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResultCommitted {
            result:
                ToolResult {
                    name,
                    status: ToolResultStatus::Success,
                    content,
                    ..
                },
            ..
        } if name == "double" && content == &vec![InputContent::Text("cancelled".to_string())]
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnPreempted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnAborted))
    );
}

#[tokio::test]
async fn turn_handle_rejects_step_while_events_driver_is_active() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();
    let mut stream = run.clone().events();

    stream.next().await.unwrap().unwrap();
    let error = run.step_until_control().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent turn already has an active driver")
    );

    drop(stream);
    run.step_until_control()
        .await
        .unwrap()
        .continue_turn()
        .await
        .unwrap();
}

#[tokio::test]
async fn turn_handle_rejects_second_events_driver() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();
    let mut first = run.clone().events();
    let mut second = run.clone().events();

    first.next().await.unwrap().unwrap();
    let error = second.next().await.unwrap().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent turn already has an active driver")
    );
}

#[tokio::test]
async fn turn_control_point_blocks_second_driver() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();
    let point = run.step_until_control().await.unwrap();

    let error = run.step_until_control().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent turn already has an active driver")
    );
    point.continue_turn().await.unwrap();
}

#[tokio::test]
async fn turn_handle_reports_illegal_control_for_step() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();
    let report = run.step_until_control().await.unwrap();
    assert!(matches!(
        report.pending_outcome(),
        AgentOutcome::ToolsLoaded(_)
    ));

    let error = report
        .control(AgentControl::ReplaceRequest(GenerateRequest {
            messages: Vec::new(),
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: None,
            options: GenerateRequestOptions::default(),
        }))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("is not valid for this step"));
}

#[tokio::test]
async fn turn_handle_replaces_model_delta_before_history() {
    let agent = test_agent(vec![
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text("secret".to_string()),
        },
        OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]);
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(report.pending_outcome(), AgentOutcome::ModelDelta { .. }) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };

    report
        .control(AgentControl::ReplaceModelDelta(OutputContentDelta::Text(
            "redacted".to_string(),
        )))
        .await
        .unwrap();
    let next = run.step_until_control().await.unwrap();
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::ModelDelta {
            delta: OutputContentDelta::Text(text),
            ..
        } if text == "redacted"
    )));

    next.continue_turn().await.unwrap();
    run.clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        agent.messages().await.unwrap(),
        vec![Message::assistant(vec![OutputContent::Text(
            "redacted".to_string()
        )])]
    );
}

#[tokio::test]
async fn control_required_precedes_final_step_completed() {
    let agent = test_agent(vec![
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text("secret".to_string()),
        },
        OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]);
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(report.pending_outcome(), AgentOutcome::ModelDelta { .. }) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };

    let report_step_id = report.step().id;
    assert!(report.events.iter().any(|event| matches!(
        event,
        AgentEvent::ControlRequired {
            step,
            outcome: AgentOutcome::ModelDelta {
                delta: OutputContentDelta::Text(text),
                ..
            },
        } if step.id == report_step_id && text == "secret"
    )));
    assert!(!report.events.iter().any(|event| matches!(
        event,
        AgentEvent::StepCompleted { step, .. } if step.id == report_step_id
    )));

    report
        .control(AgentControl::ReplaceModelDelta(OutputContentDelta::Text(
            "redacted".to_string(),
        )))
        .await
        .unwrap();
    let next = run.step_until_control().await.unwrap();
    let completed_index = next
        .events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::StepCompleted {
                    step,
                    outcome: AgentOutcome::ModelDelta {
                        delta: OutputContentDelta::Text(text),
                        ..
                    },
                } if step.id == report_step_id && text == "redacted"
            )
        })
        .expect("final step completion emitted");
    let delta_index = next
        .events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::ModelDelta {
                    delta: OutputContentDelta::Text(text),
                    ..
                } if text == "redacted"
            )
        })
        .expect("rewritten delta emitted");
    assert!(completed_index < delta_index);
}

#[tokio::test]
async fn turn_handle_drops_model_delta_before_history() {
    let agent = test_agent(vec![
        OutputStreamEvent::Delta {
            content_index: 0,
            delta: OutputContentDelta::Text("secret".to_string()),
        },
        OutputStreamEvent::Finished {
            reason: FinishReason::Stop,
            usage: None,
        },
    ]);
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(report.pending_outcome(), AgentOutcome::ModelDelta { .. }) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };

    report.control(AgentControl::DropModelDelta).await.unwrap();
    let events = run
        .clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::ModelDelta {
            delta: OutputContentDelta::Text(text),
            ..
        } if text == "secret"
    )));
    assert_eq!(
        agent.messages().await.unwrap(),
        vec![Message::assistant(Vec::new())]
    );
}

#[tokio::test]
async fn turn_handle_replaces_request_before_model_stream() {
    let captured = Arc::new(Mutex::new(None));
    let model = Arc::new(CapturingModel {
        captured: Arc::clone(&captured),
    });
    let agent = TestSession::new(model, Arc::new(EmptyToolRouter));
    let replacement = GenerateRequest {
        messages: vec![Message::system(vec![InputContent::Text(
            "replacement".to_string(),
        )])],
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: Some(ToolChoice::None),
        options: GenerateRequestOptions {
            max_tokens: Some(7),
            ..GenerateRequestOptions::default()
        },
    };
    let run = agent.start_turn().await.unwrap();

    let point = loop {
        let point = run.step_until_control().await.unwrap();
        if matches!(point.checkpoint(), AgentCheckpoint::RequestBuilt(_)) {
            break point;
        }
        point.continue_turn().await.unwrap();
    };
    point
        .control(AgentControl::ReplaceRequest(replacement.clone()))
        .await
        .unwrap();
    run.clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    assert_eq!(captured.lock().unwrap().take(), Some(replacement));
}

#[tokio::test]
async fn turn_handle_replaces_assistant_output_before_commit() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(
            report.pending_outcome(),
            AgentOutcome::ModelOutputFinished { .. }
        ) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };

    report
        .control(AgentControl::ReplaceAssistantOutput(vec![
            OutputContent::Text("redacted".to_string()),
        ]))
        .await
        .unwrap();
    let next = run.step_until_control().await.unwrap();
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::AssistantCommitted { content, .. }
            if content == &vec![OutputContent::Text("redacted".to_string())]
    )));
    next.continue_turn().await.unwrap();
    run.clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        agent.messages().await.unwrap(),
        vec![Message::assistant(vec![OutputContent::Text(
            "redacted".to_string()
        )])]
    );
}

#[tokio::test]
async fn turn_handle_replaces_tool_result_before_commit() {
    let agent = TestSession::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );
    let run = agent.start_turn().await.unwrap();

    let point = loop {
        let point = run.step_until_control().await.unwrap();
        if matches!(point.checkpoint(), AgentCheckpoint::ToolResult(_)) {
            break point;
        }
        point.continue_turn().await.unwrap();
    };
    let replacement = ToolResultReplacement {
        status: ToolResultStatus::Error,
        content: vec![InputContent::Text("blocked".to_string())],
    };
    let expected = ToolResult {
        call_id: "call-1".into(),
        name: "double".to_string(),
        status: ToolResultStatus::Error,
        content: vec![InputContent::Text("blocked".to_string())],
    };

    point
        .control(AgentControl::ReplaceToolResultContent(replacement))
        .await
        .unwrap();
    let next = run.step_until_control().await.unwrap();
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResultCommitted { result, .. } if result == &expected
    )));
    next.continue_turn().await.unwrap();
    run.clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert!(
        agent
            .messages()
            .await
            .unwrap()
            .iter()
            .any(|message| matches!(message, Message::Output(OutputMessage::Tool(result)) if result == &expected))
    );
}

#[tokio::test]
async fn turn_handle_rejects_mismatched_tool_result_replacement_immediately() {
    let agent = TestSession::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(
            report.pending_outcome(),
            AgentOutcome::ToolResultCommitted { .. }
        ) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };
    let replacement = ToolResult {
        call_id: "wrong-call".into(),
        name: "double".to_string(),
        status: ToolResultStatus::Error,
        content: vec![InputContent::Text("blocked".to_string())],
    };

    let error = report
        .control(AgentControl::ReplaceToolResult(replacement))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("must keep the original call_id and name")
    );
}

#[tokio::test]
async fn turn_handle_rejects_duplicate_tool_call_replacement_immediately() {
    let agent = TestSession::new(
        Arc::new(MultiToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(ConcurrentToolRouter::default()),
    );
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(report.pending_outcome(), AgentOutcome::ToolPlanned { .. }) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };
    let replacement = ToolCall {
        id: "call-p2".into(),
        name: "p1".to_string(),
        arguments: serde_json::Map::new(),
    };

    let error = report
        .control(AgentControl::ReplaceToolCall(replacement))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("replacement tool call id must be unique")
    );
}

#[tokio::test]
async fn turn_handle_rejects_assistant_output_inconsistent_with_finish_reason() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(
            report.pending_outcome(),
            AgentOutcome::ModelOutputFinished { .. }
        ) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };
    let replacement = vec![OutputContent::ToolCall(ToolCall {
        id: "call-1".into(),
        name: "double".to_string(),
        arguments: serde_json::Map::new(),
    })];

    let error = report
        .control(AgentControl::ReplaceAssistantOutput(replacement))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("finish reason Stop cannot contain tool calls")
    );
}

#[tokio::test]
async fn turn_handle_replaces_tool_call_before_execution() {
    let agent = TestSession::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );
    let mut replacement_arguments = serde_json::Map::new();
    replacement_arguments.insert("value".to_string(), Value::from(5));
    let replacement = ToolCall {
        id: "call-2".into(),
        name: "double".to_string(),
        arguments: replacement_arguments.clone(),
    };
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(report.pending_outcome(), AgentOutcome::ToolPlanned { .. }) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };

    report
        .control(AgentControl::ReplaceToolCall(replacement.clone()))
        .await
        .unwrap();
    let events = run
        .clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    let stream_events = stream_events(&events);

    assert!(stream_events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolStarted(ToolCall { id, name, arguments })
            if id.as_str() == "call-2"
                && name == "double"
                && arguments == &replacement_arguments
    )));
    assert!(stream_events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolResultCommitted(ToolResult {
            call_id,
            name,
            status: ToolResultStatus::Success,
            content,
        }) if call_id.as_str() == "call-2"
            && name == "double"
            && content == &vec![InputContent::Text("10".to_string())]
    )));
    assert!(matches!(
        agent.messages().await.unwrap().as_slice(),
        [
            Message::Output(OutputMessage::Assistant(content)),
            Message::Output(OutputMessage::Tool(ToolResult {
                call_id,
                name,
                status: ToolResultStatus::Success,
                content: result_content,
            })),
            Message::Output(OutputMessage::Assistant(done_content)),
        ] if matches!(content.as_slice(), [OutputContent::ToolCall(ToolCall { id, name, arguments })]
            if id.as_str() == "call-2"
                && name == "double"
                && arguments == &replacement_arguments)
            && call_id.as_str() == "call-2"
            && name == "double"
            && result_content == &vec![InputContent::Text("10".to_string())]
            && done_content == &vec![OutputContent::Text("done".to_string())]
    ));
}

#[tokio::test]
async fn turn_handle_rejects_tool_call_before_execution() {
    let agent = TestSession::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );
    let run = agent.start_turn().await.unwrap();

    let report = loop {
        let report = run.step_until_control().await.unwrap();
        if matches!(report.pending_outcome(), AgentOutcome::ToolPlanned { .. }) {
            break report;
        }
        report.continue_turn().await.unwrap();
    };

    report
        .control(AgentControl::RejectToolCall {
            reason: "blocked by policy".to_string(),
        })
        .await
        .unwrap();
    let events = run
        .clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    let stream_events = stream_events(&events);

    assert!(
        !stream_events
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolStarted(_)))
    );
    assert!(stream_events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolResultCommitted(ToolResult {
            call_id,
            name,
            status: ToolResultStatus::Error,
            content,
        }) if call_id.as_str() == "call-1"
            && name == "double"
            && content == &vec![InputContent::Text("blocked by policy".to_string())]
    )));
    assert!(matches!(
        agent.messages().await.unwrap().as_slice(),
        [
            Message::Output(OutputMessage::Assistant(_)),
            Message::Output(OutputMessage::Tool(ToolResult {
                call_id,
                name,
                status: ToolResultStatus::Error,
                content,
            })),
            Message::Output(OutputMessage::Assistant(done_content)),
        ] if call_id.as_str() == "call-1"
            && name == "double"
            && content == &vec![InputContent::Text("blocked by policy".to_string())]
            && done_content == &vec![OutputContent::Text("done".to_string())]
    ));
}

fn test_agent(events: Vec<OutputStreamEvent>) -> TestSession {
    TestSession::new(Arc::new(TestModel { events }), Arc::new(EmptyToolRouter))
}

#[tokio::test]
async fn build_request_carries_agent_baseline_config() {
    let captured = Arc::new(Mutex::new(None));
    let model = Arc::new(CapturingModel {
        captured: Arc::clone(&captured),
    });
    let agent = TestSession::new(model, Arc::new(EmptyToolRouter));
    agent
        .replace_messages(vec![Message::user(vec![InputContent::Text(
            "hi".to_string(),
        )])])
        .await
        .unwrap();
    agent
        .set_options(GenerateRequestOptions {
            temperature: Some(0.5),
            max_tokens: Some(256),
            ..GenerateRequestOptions::default()
        })
        .await
        .unwrap();
    agent
        .set_tool_choice(Some(ToolChoice::Required))
        .await
        .unwrap();
    agent
        .set_hosted_tools(vec![HostedToolSpec::new("web_search")])
        .await
        .unwrap();

    collect_agent_events(&agent).await;

    let request = captured.lock().unwrap().take().expect("request captured");
    assert_eq!(request.options.temperature, Some(0.5));
    assert_eq!(request.options.max_tokens, Some(256));
    assert_eq!(request.tool_choice, Some(ToolChoice::Required));
    assert_eq!(
        request.hosted_tools,
        vec![HostedToolSpec::new("web_search")]
    );
}

#[tokio::test]
async fn agent_history_and_turn_config_round_trip_state() {
    let mut options = GenerateRequestOptions {
        temperature: Some(0.7),
        max_tokens: Some(512),
        ..GenerateRequestOptions::default()
    };
    options
        .insert_extra(serde_json::json!({"provider": "test"}))
        .unwrap();
    let hosted_tool = HostedToolSpec::new("web_search")
        .with_parameters(serde_json::json!({"region": "global"}))
        .unwrap();
    let expected_history =
        AgentHistory::from_messages(vec![Message::user(vec![InputContent::Text(
            "hi".to_string(),
        )])]);
    let expected_config = AgentTurnConfig::new(
        options,
        Some(ToolChoice::Specific {
            name: "double".to_string(),
        }),
        vec![hosted_tool],
    );
    let agent = TestSession::from_parts(
        expected_history.clone(),
        expected_config.clone(),
        Arc::new(TestModel { events: Vec::new() }),
        Arc::new(EmptyToolRouter),
    );

    let history = agent.history().await.unwrap();
    assert_eq!(history, expected_history);
    assert_eq!(history.messages(), expected_history.messages());
    let encoded_history = serde_json::to_value(&history).unwrap();
    let decoded_history: AgentHistory = serde_json::from_value(encoded_history).unwrap();
    assert_eq!(decoded_history, history);

    let config = agent.config().await.unwrap();
    assert_eq!(config, expected_config);
    assert_eq!(config.options(), expected_config.options());
    assert_eq!(config.tool_choice(), expected_config.tool_choice());
    assert_eq!(config.hosted_tools(), expected_config.hosted_tools());
    let encoded_config = serde_json::to_value(&config).unwrap();
    let decoded_config: AgentTurnConfig = serde_json::from_value(encoded_config).unwrap();
    assert_eq!(decoded_config, config);
}

#[tokio::test]
async fn agent_from_history_restores_independent_history() {
    let source = test_agent(Vec::new());
    source
        .replace_messages(vec![Message::user(vec![InputContent::Text(
            "source".to_string(),
        )])])
        .await
        .unwrap();
    source
        .set_tool_choice(Some(ToolChoice::Required))
        .await
        .unwrap();
    let history = source.history().await.unwrap();
    let config = source.config().await.unwrap();
    let restored = TestSession::from_parts(
        history.clone(),
        config,
        Arc::new(TestModel { events: Vec::new() }),
        Arc::new(EmptyToolRouter),
    );

    assert_eq!(restored.history().await.unwrap(), history);
    restored
        .push_input(InputMessage::User(vec![InputContent::Text(
            "child".to_string(),
        )]))
        .await
        .unwrap();

    assert_eq!(source.history().await.unwrap(), history);
    assert_eq!(restored.messages().await.unwrap().len(), 2);
}

struct CapturingModel {
    captured: Arc<Mutex<Option<GenerateRequest>>>,
}

impl Model for CapturingModel {
    fn stream(&self, request: GenerateRequest) -> ModelStream {
        *self.captured.lock().unwrap() = Some(request);
        Box::pin(futures_util::stream::iter(
            vec![OutputStreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            }]
            .into_iter()
            .map(Ok),
        ))
    }
}

struct EmptyToolRouter;

#[async_trait]
impl ToolRouter for EmptyToolRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        Ok(Vec::new())
    }

    async fn execute(&self, _call: ToolCall, _cancel: CancellationToken) -> Result<ToolResult> {
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

    async fn execute(&self, call: ToolCall, _cancel: CancellationToken) -> Result<ToolResult> {
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

struct PanicToolRouter;

#[async_trait]
impl ToolRouter for PanicToolRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        DoubleToolRouter.definitions().await
    }

    async fn execute(&self, _call: ToolCall, _cancel: CancellationToken) -> Result<ToolResult> {
        panic!("boom")
    }
}

struct PreemptibleToolRouter;

#[async_trait]
impl ToolRouter for PreemptibleToolRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        DoubleToolRouter.definitions().await
    }

    async fn execute(&self, call: ToolCall, cancel: CancellationToken) -> Result<ToolResult> {
        cancel.cancelled().await;
        Ok(ToolResult {
            call_id: call.id,
            name: call.name,
            status: ToolResultStatus::Success,
            content: vec![InputContent::Text("cancelled".to_string())],
        })
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

    async fn execute(&self, call: ToolCall, _cancel: CancellationToken) -> Result<ToolResult> {
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
            let delay = match call.name.as_str() {
                "p1" | "p3" => Duration::from_millis(30),
                "p2" | "p4" => Duration::from_millis(1),
                _ => Duration::from_millis(10),
            };
            tokio::time::sleep(delay).await;
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
    fn stream(&self, _request: GenerateRequest) -> ModelStream {
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
    fn stream(&self, _request: GenerateRequest) -> ModelStream {
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

struct PendingAfterFirstDeltaModel;

impl Model for PendingAfterFirstDeltaModel {
    fn stream(&self, _request: GenerateRequest) -> ModelStream {
        let first = futures_util::stream::once(async {
            Ok(OutputStreamEvent::Delta {
                content_index: 0,
                delta: OutputContentDelta::Text("first".to_string()),
            })
        });
        Box::pin(first.chain(futures_util::stream::pending()))
    }
}

struct DelayedSecondDeltaModel;

impl Model for DelayedSecondDeltaModel {
    fn stream(&self, _request: GenerateRequest) -> ModelStream {
        Box::pin(futures_util::stream::unfold(0, |index| async move {
            match index {
                0 => Some((
                    Ok(OutputStreamEvent::Delta {
                        content_index: 0,
                        delta: OutputContentDelta::Text("first".to_string()),
                    }),
                    1,
                )),
                1 => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Some((
                        Ok(OutputStreamEvent::Delta {
                            content_index: 0,
                            delta: OutputContentDelta::Text("second".to_string()),
                        }),
                        2,
                    ))
                }
                2 => Some((
                    Ok(OutputStreamEvent::Finished {
                        reason: FinishReason::Stop,
                        usage: None,
                    }),
                    3,
                )),
                _ => None,
            }
        }))
    }
}

struct TestModel {
    events: Vec<OutputStreamEvent>,
}

impl Model for TestModel {
    fn stream(&self, _request: GenerateRequest) -> ModelStream {
        Box::pin(futures_util::stream::iter(
            self.events.clone().into_iter().map(Ok),
        ))
    }
}
