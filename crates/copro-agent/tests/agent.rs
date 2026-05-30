use copro_agent::{
    Agent, AgentAction, AgentControl, AgentControlPoint, AgentEvent, AgentOutcome, AgentRunState,
    CancellationToken, StopSignal, ToolExecutionPolicy, ToolResultReplacement, ToolRouter,
    async_trait,
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
                reason: reason.clone(),
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

#[tokio::test]
async fn run_stream_commits_assistant_message() {
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
        .replace_messages(vec![Message::User(vec![InputContent::Text(
            "hi".to_string(),
        )])])
        .await
        .unwrap();

    let events = agent
        .run_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    let assistant = Message::Assistant(vec![OutputContent::Text("Hello".to_string())]);
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
            Message::User(vec![InputContent::Text("hi".to_string())]),
            assistant,
        ]
    );
}

#[tokio::test]
async fn run_stream_stops_when_stop_signal_is_requested() {
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
    let stop_signal = agent.stop_signal();

    {
        let mut stream = agent.run_stream();
        loop {
            match stream.next().await.transpose().unwrap() {
                Some(AgentEvent::ModelDelta {
                    delta: OutputContentDelta::Text(text),
                    ..
                }) if text == "first" => break,
                Some(_) => {}
                None => panic!("stream ended before first model delta"),
            }
        }

        stop_signal.request_stop();
        let remaining = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(stream_events(&remaining).is_empty());
    }

    assert!(agent.messages().await.unwrap().is_empty());
}

#[tokio::test]
async fn run_stream_interrupts_pending_model_stream() {
    let agent = Agent::new(
        Arc::new(PendingAfterFirstDeltaModel),
        Arc::new(EmptyToolRouter),
    );
    let stop_signal = agent.stop_signal();

    {
        let mut stream = agent.run_stream();
        loop {
            match stream.next().await.transpose().unwrap() {
                Some(AgentEvent::ModelDelta {
                    delta: OutputContentDelta::Text(text),
                    ..
                }) if text == "first" => break,
                Some(_) => {}
                None => panic!("stream ended before first model delta"),
            }
        }

        stop_signal.request_stop();
        let remaining =
            tokio::time::timeout(Duration::from_millis(100), stream.collect::<Vec<_>>())
                .await
                .unwrap();
        let remaining = remaining.into_iter().collect::<Result<Vec<_>>>().unwrap();
        assert!(stream_events(&remaining).is_empty());
    }

    assert!(agent.messages().await.unwrap().is_empty());
}

#[tokio::test]
async fn stop_after_tool_call_commit_records_aborted_tool_result() {
    let agent = Agent::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );
    let stop_signal = agent.stop_signal();

    {
        let mut stream = agent.run_stream();
        loop {
            match stream.next().await.transpose().unwrap() {
                Some(AgentEvent::AssistantCommitted {
                    reason: FinishReason::ToolCalls,
                    ..
                }) => break,
                Some(_) => {}
                None => panic!("stream ended before assistant tool call commit"),
            }
        }

        stop_signal.request_stop();
        let remaining = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(stream_events(&remaining).iter().any(|event| matches!(
            event,
            StreamEvent::ToolResultCommitted(ToolResult {
                name,
                status: ToolResultStatus::Error,
                content,
                ..
            }) if name == "double"
                && content == &vec![InputContent::Text("aborted by user".to_string())]
        )));
    }

    assert!(matches!(
        agent.messages().await.unwrap().as_slice(),
        [
            Message::Assistant(content),
            Message::Tool(ToolResult {
                call_id,
                name,
                status: ToolResultStatus::Error,
                content: result_content,
            })
        ] if matches!(content.as_slice(), [OutputContent::ToolCall(ToolCall { id, name, .. })]
            if id.as_str() == "call-1" && name == "double")
            && call_id.as_str() == "call-1"
            && name == "double"
            && result_content == &vec![InputContent::Text("aborted by user".to_string())]
    ));
}

#[tokio::test]
async fn stop_during_tool_execution_still_commits_tool_result() {
    let model = Arc::new(ToolThenDoneModel {
        calls: AtomicUsize::new(0),
    });
    let stop_signal = StopSignal::new();
    let agent = Agent::with_stop_signal(
        model.clone(),
        Arc::new(StopDuringToolRouter {
            stop_signal: stop_signal.clone(),
        }),
        stop_signal,
    );

    let events = agent
        .run_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    let stream_events = stream_events(&events);
    assert!(stream_events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolResultCommitted(ToolResult {
            name,
            status: ToolResultStatus::Success,
            content,
            ..
        }) if name == "double" && content == &vec![InputContent::Text("42".to_string())]
    )));
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        agent.messages().await.unwrap().as_slice(),
        [
            Message::Assistant(_),
            Message::Tool(ToolResult {
                status: ToolResultStatus::Success,
                ..
            })
        ]
    ));
}

#[tokio::test]
async fn run_stream_awaits_async_tool_router() {
    let agent = Agent::new(
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
    let agent = Agent::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(PanicToolRouter),
    );

    let events = agent
        .run_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

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
            Message::Assistant(assistant_content),
            Message::Tool(ToolResult {
                call_id,
                name,
                status: ToolResultStatus::Error,
                content,
            }),
            Message::Assistant(done_content),
        ] if matches!(assistant_content.as_slice(), [OutputContent::ToolCall(ToolCall { id, name, .. })]
            if id.as_str() == "call-1" && name == "double")
            && call_id.as_str() == "call-1"
            && name == "double"
            && content == &vec![InputContent::Text("tool task panicked: boom".to_string())]
            && done_content == &vec![OutputContent::Text("done".to_string())]
    ));
}

#[tokio::test]
async fn run_stream_batches_parallel_tools_behind_serial_barriers() {
    let router = Arc::new(ConcurrentToolRouter::default());
    let tool_router: Arc<dyn ToolRouter> = router.clone();
    let agent = Agent::new(
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
async fn start_run_steps_to_model_delta_boundary() {
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
    let run = agent.start_run().await.unwrap();

    let mut report = run.step().await.unwrap();
    while !matches!(report.outcome, AgentOutcome::ModelDelta { .. }) {
        report = run.step().await.unwrap();
    }

    assert!(matches!(report.step.action, AgentAction::ReadModelStream));
    assert_eq!(report.step.id.tick, 3);
    assert!(matches!(
        report.outcome,
        AgentOutcome::ModelDelta {
            content_index: 0,
            delta: OutputContentDelta::Text(ref text),
        } if text == "Hello"
    ));
    assert!(matches!(
        run.state().await.unwrap(),
        AgentRunState::WaitingControl { .. }
    ));
    run.control(report.step.id, AgentControl::Continue)
        .await
        .unwrap();
    let next = run.step().await.unwrap();
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::ModelDelta {
            delta: OutputContentDelta::Text(text),
            ..
        } if text == "Hello"
    )));
}

#[tokio::test]
async fn run_handle_pause_and_resume_at_boundary() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_run().await.unwrap();

    let first = run.step().await.unwrap();
    run.pause().await.unwrap();
    assert_eq!(
        run.state().await.unwrap(),
        AgentRunState::Paused { at: first.step.id }
    );

    run.resume().await.unwrap();
    let second = run.step().await.unwrap();
    assert!(second.events.iter().any(|event| matches!(
        event,
        AgentEvent::RunPaused { at, .. } if *at == first.step.id
    )));
    assert!(second.events.iter().any(|event| matches!(
        event,
        AgentEvent::RunResumed { at, .. } if *at == first.step.id
    )));
    assert_eq!(second.step.id.tick, first.step.id.tick + 1);
}

#[tokio::test]
async fn run_handle_control_pause_emits_pause_and_resume_events() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_run().await.unwrap();

    let first = run.step().await.unwrap();
    run.control(first.step.id, AgentControl::Pause)
        .await
        .unwrap();
    assert_eq!(
        run.state().await.unwrap(),
        AgentRunState::Paused { at: first.step.id }
    );

    run.resume().await.unwrap();
    let second = run.step().await.unwrap();
    assert!(second.events.iter().any(|event| matches!(
        event,
        AgentEvent::RunPaused { at, .. } if *at == first.step.id
    )));
    assert!(second.events.iter().any(|event| matches!(
        event,
        AgentEvent::RunResumed { at, .. } if *at == first.step.id
    )));
    assert_eq!(second.step.id.tick, first.step.id.tick + 1);
}

#[tokio::test]
async fn run_handle_pause_request_waits_for_next_boundary() {
    let agent = Agent::new(Arc::new(DelayedSecondDeltaModel), Arc::new(EmptyToolRouter));
    let run = agent.start_run().await.unwrap();

    let first = loop {
        let report = run.step().await.unwrap();
        if matches!(
            report.outcome,
            AgentOutcome::ModelDelta {
                delta: OutputContentDelta::Text(ref text),
                ..
            } if text == "first"
        ) {
            break report;
        }
        run.control(report.step.id, AgentControl::Continue)
            .await
            .unwrap();
    };
    run.control(first.step.id, AgentControl::Continue)
        .await
        .unwrap();

    let step_task = tokio::spawn({
        let run = run.clone();
        async move { run.step().await }
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    run.pause().await.unwrap();

    let paused = step_task.await.unwrap().unwrap();
    assert!(matches!(
        paused.outcome,
        AgentOutcome::ModelDelta {
            delta: OutputContentDelta::Text(ref text),
            ..
        } if text == "second"
    ));
    assert_eq!(paused.state, AgentRunState::Paused { at: paused.step.id });

    run.resume().await.unwrap();
    let next = run.step().await.unwrap();
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::RunPaused { at, .. } if *at == paused.step.id
    )));
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::RunResumed { at, .. } if *at == paused.step.id
    )));
}

#[tokio::test]
async fn run_handle_rejects_stale_control() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_run().await.unwrap();
    let report = run.step().await.unwrap();
    let mut stale_step_id = report.step.id;
    stale_step_id.tick += 1;

    let error = run
        .control(stale_step_id, AgentControl::Continue)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stale agent control step id"));
}

#[tokio::test]
async fn run_handle_reports_illegal_control_for_step() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_run().await.unwrap();
    let report = run.step().await.unwrap();
    assert!(matches!(report.outcome, AgentOutcome::ToolsLoaded(_)));

    let error = run
        .control(
            report.step.id,
            AgentControl::ReplaceRequest(GenerateRequest {
                messages: Vec::new(),
                tools: Vec::new(),
                hosted_tools: Vec::new(),
                tool_choice: None,
                options: GenerateRequestOptions::default(),
            }),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("is not valid for this step"));
    run.control(report.step.id, AgentControl::Continue)
        .await
        .unwrap();
}

#[tokio::test]
async fn run_handle_replaces_model_delta_before_history() {
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
    let run = agent.start_run().await.unwrap();

    let report = loop {
        let report = run.step().await.unwrap();
        if matches!(report.outcome, AgentOutcome::ModelDelta { .. }) {
            break report;
        }
        run.control(report.step.id, AgentControl::Continue)
            .await
            .unwrap();
    };

    run.control(
        report.step.id,
        AgentControl::ReplaceModelDelta(OutputContentDelta::Text("redacted".to_string())),
    )
    .await
    .unwrap();
    let next = run.step().await.unwrap();
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::ModelDelta {
            delta: OutputContentDelta::Text(text),
            ..
        } if text == "redacted"
    )));

    run.control(next.step.id, AgentControl::Continue)
        .await
        .unwrap();
    run.clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        agent.messages().await.unwrap(),
        vec![Message::Assistant(vec![OutputContent::Text(
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
    let run = agent.start_run().await.unwrap();

    let report = loop {
        let report = run.step().await.unwrap();
        if matches!(report.outcome, AgentOutcome::ModelDelta { .. }) {
            break report;
        }
        run.control(report.step.id, AgentControl::Continue)
            .await
            .unwrap();
    };

    assert!(report.events.iter().any(|event| matches!(
        event,
        AgentEvent::ControlRequired {
            step,
            outcome: AgentOutcome::ModelDelta {
                delta: OutputContentDelta::Text(text),
                ..
            },
        } if step.id == report.step.id && text == "secret"
    )));
    assert!(!report.events.iter().any(|event| matches!(
        event,
        AgentEvent::StepCompleted { step, .. } if step.id == report.step.id
    )));

    run.control(
        report.step.id,
        AgentControl::ReplaceModelDelta(OutputContentDelta::Text("redacted".to_string())),
    )
    .await
    .unwrap();
    let next = run.step().await.unwrap();
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
                } if step.id == report.step.id && text == "redacted"
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
async fn run_handle_drops_model_delta_before_history() {
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
    let run = agent.start_run().await.unwrap();

    let report = loop {
        let report = run.step().await.unwrap();
        if matches!(report.outcome, AgentOutcome::ModelDelta { .. }) {
            break report;
        }
        run.control(report.step.id, AgentControl::Continue)
            .await
            .unwrap();
    };

    run.control(report.step.id, AgentControl::DropModelDelta)
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
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::ModelDelta {
            delta: OutputContentDelta::Text(text),
            ..
        } if text == "secret"
    )));
    assert_eq!(
        agent.messages().await.unwrap(),
        vec![Message::Assistant(Vec::new())]
    );
}

#[tokio::test]
async fn run_handle_replaces_request_before_model_stream() {
    let captured = Arc::new(Mutex::new(None));
    let model = Arc::new(CapturingModel {
        captured: Arc::clone(&captured),
    });
    let agent = Agent::new(model, Arc::new(EmptyToolRouter));
    let replacement = GenerateRequest {
        messages: vec![Message::System(vec![InputContent::Text(
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
    let run = agent.start_run().await.unwrap();

    let point = loop {
        let point = run.step_until_control().await.unwrap();
        if matches!(point, AgentControlPoint::RequestBuilt(_)) {
            break point;
        }
        run.apply_control(point.continue_run()).await.unwrap();
    };
    let AgentControlPoint::RequestBuilt(point) = point else {
        unreachable!("request control point expected")
    };

    run.apply_control(point.replace_request(replacement.clone()))
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
async fn run_handle_replaces_assistant_output_before_commit() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_run().await.unwrap();

    let report = loop {
        let report = run.step().await.unwrap();
        if matches!(report.outcome, AgentOutcome::ModelOutputFinished { .. }) {
            break report;
        }
        run.control(report.step.id, AgentControl::Continue)
            .await
            .unwrap();
    };

    run.control(
        report.step.id,
        AgentControl::ReplaceAssistantOutput(vec![OutputContent::Text("redacted".to_string())]),
    )
    .await
    .unwrap();
    let next = run.step().await.unwrap();
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::AssistantCommitted { content, .. }
            if content == &vec![OutputContent::Text("redacted".to_string())]
    )));
    run.control(next.step.id, AgentControl::Continue)
        .await
        .unwrap();
    run.clone()
        .events()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        agent.messages().await.unwrap(),
        vec![Message::Assistant(vec![OutputContent::Text(
            "redacted".to_string()
        )])]
    );
}

#[tokio::test]
async fn run_handle_replaces_tool_result_before_commit() {
    let agent = Agent::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );
    let run = agent.start_run().await.unwrap();

    let point = loop {
        let point = run.step_until_control().await.unwrap();
        if matches!(point, AgentControlPoint::ToolResult(_)) {
            break point;
        }
        run.apply_control(point.continue_run()).await.unwrap();
    };
    let AgentControlPoint::ToolResult(point) = point else {
        unreachable!("tool result control point expected")
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

    run.apply_control(point.replace_tool_result(replacement))
        .await
        .unwrap();
    let next = run.step().await.unwrap();
    assert!(next.events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResultCommitted { result, .. } if result == &expected
    )));
    run.control(next.step.id, AgentControl::Continue)
        .await
        .unwrap();
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
            .any(|message| matches!(message, Message::Tool(result) if result == &expected))
    );
}

#[tokio::test]
async fn run_handle_rejects_mismatched_tool_result_replacement_immediately() {
    let agent = Agent::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );
    let run = agent.start_run().await.unwrap();

    let report = loop {
        let report = run.step().await.unwrap();
        if matches!(report.outcome, AgentOutcome::ToolResultCommitted { .. }) {
            break report;
        }
        run.control(report.step.id, AgentControl::Continue)
            .await
            .unwrap();
    };
    let replacement = ToolResult {
        call_id: "wrong-call".into(),
        name: "double".to_string(),
        status: ToolResultStatus::Error,
        content: vec![InputContent::Text("blocked".to_string())],
    };

    let error = run
        .control(report.step.id, AgentControl::ReplaceToolResult(replacement))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("must keep the original call_id and name")
    );
    run.control(report.step.id, AgentControl::Continue)
        .await
        .unwrap();
}

#[tokio::test]
async fn run_handle_rejects_duplicate_tool_call_replacement_immediately() {
    let agent = Agent::new(
        Arc::new(MultiToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(ConcurrentToolRouter::default()),
    );
    let run = agent.start_run().await.unwrap();

    let report = loop {
        let report = run.step().await.unwrap();
        if matches!(report.outcome, AgentOutcome::ToolPlanned { .. }) {
            break report;
        }
        run.control(report.step.id, AgentControl::Continue)
            .await
            .unwrap();
    };
    let replacement = ToolCall {
        id: "call-p2".into(),
        name: "p1".to_string(),
        arguments: serde_json::Map::new(),
    };

    let error = run
        .control(report.step.id, AgentControl::ReplaceToolCall(replacement))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("replacement tool call id must be unique")
    );
    run.control(report.step.id, AgentControl::Continue)
        .await
        .unwrap();
}

#[tokio::test]
async fn run_handle_rejects_assistant_output_inconsistent_with_finish_reason() {
    let agent = test_agent(vec![OutputStreamEvent::Finished {
        reason: FinishReason::Stop,
        usage: None,
    }]);
    let run = agent.start_run().await.unwrap();

    let report = loop {
        let report = run.step().await.unwrap();
        if matches!(report.outcome, AgentOutcome::ModelOutputFinished { .. }) {
            break report;
        }
        run.control(report.step.id, AgentControl::Continue)
            .await
            .unwrap();
    };
    let replacement = vec![OutputContent::ToolCall(ToolCall {
        id: "call-1".into(),
        name: "double".to_string(),
        arguments: serde_json::Map::new(),
    })];

    let error = run
        .control(
            report.step.id,
            AgentControl::ReplaceAssistantOutput(replacement),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("finish reason Stop cannot contain tool calls")
    );
    run.control(report.step.id, AgentControl::Continue)
        .await
        .unwrap();
}

#[tokio::test]
async fn run_handle_replaces_tool_call_before_execution() {
    let agent = Agent::new(
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
    let run = agent.start_run().await.unwrap();

    let report = loop {
        let report = run.step().await.unwrap();
        if matches!(report.outcome, AgentOutcome::ToolPlanned { .. }) {
            break report;
        }
        run.control(report.step.id, AgentControl::Continue)
            .await
            .unwrap();
    };

    run.control(
        report.step.id,
        AgentControl::ReplaceToolCall(replacement.clone()),
    )
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
            Message::Assistant(content),
            Message::Tool(ToolResult {
                call_id,
                name,
                status: ToolResultStatus::Success,
                content: result_content,
            }),
            Message::Assistant(done_content),
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
async fn run_handle_rejects_tool_call_before_execution() {
    let agent = Agent::new(
        Arc::new(ToolThenDoneModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(DoubleToolRouter),
    );
    let run = agent.start_run().await.unwrap();

    let report = loop {
        let report = run.step().await.unwrap();
        if matches!(report.outcome, AgentOutcome::ToolPlanned { .. }) {
            break report;
        }
        run.control(report.step.id, AgentControl::Continue)
            .await
            .unwrap();
    };

    run.control(
        report.step.id,
        AgentControl::RejectToolCall {
            reason: "blocked by policy".to_string(),
        },
    )
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
            Message::Assistant(_),
            Message::Tool(ToolResult {
                call_id,
                name,
                status: ToolResultStatus::Error,
                content,
            }),
            Message::Assistant(done_content),
        ] if call_id.as_str() == "call-1"
            && name == "double"
            && content == &vec![InputContent::Text("blocked by policy".to_string())]
            && done_content == &vec![OutputContent::Text("done".to_string())]
    ));
}

fn test_agent(events: Vec<OutputStreamEvent>) -> Agent {
    Agent::new(Arc::new(TestModel { events }), Arc::new(EmptyToolRouter))
}

#[tokio::test]
async fn build_request_carries_agent_baseline_config() {
    let captured = Arc::new(Mutex::new(None));
    let model = Arc::new(CapturingModel {
        captured: Arc::clone(&captured),
    });
    let agent = Agent::new(model, Arc::new(EmptyToolRouter));
    agent
        .replace_messages(vec![Message::User(vec![InputContent::Text(
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

    agent
        .run_stream()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

    let request = captured.lock().unwrap().take().expect("request captured");
    assert_eq!(request.options.temperature, Some(0.5));
    assert_eq!(request.options.max_tokens, Some(256));
    assert_eq!(request.tool_choice, Some(ToolChoice::Required));
    assert_eq!(
        request.hosted_tools,
        vec![HostedToolSpec::new("web_search")]
    );
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

struct StopDuringToolRouter {
    stop_signal: StopSignal,
}

#[async_trait]
impl ToolRouter for StopDuringToolRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        DoubleToolRouter.definitions().await
    }

    async fn execute(&self, call: ToolCall, _cancel: CancellationToken) -> Result<ToolResult> {
        self.stop_signal.request_stop();
        Ok(ToolResult {
            call_id: call.id,
            name: call.name,
            status: ToolResultStatus::Success,
            content: vec![InputContent::Text("42".to_string())],
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
