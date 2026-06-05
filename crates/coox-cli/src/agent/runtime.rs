use copro_agent::{
    AgentEvent, AgentHistory, AgentOutcome, AgentTurnConfig, AgentTurnHandle, AgentTurnState,
    ToolRouter, start_turn,
};
use copro_api::message::InputMessage;
use copro_api::stream::Model;
use futures_util::StreamExt;
use std::sync::{Arc, Mutex};
use std::{error::Error as StdError, fmt};
use tokio::sync::mpsc;

struct RuntimeState {
    turn: RuntimeTurn,
}

#[derive(Default)]
enum RuntimeTurn {
    #[default]
    Idle,
    Ready {
        history: AgentHistory,
    },
    Running {
        handle: AgentTurnHandle,
        abort_requested: bool,
    },
    PendingAck {
        history: AgentHistory,
    },
    Failed {
        history: AgentHistory,
    },
}

impl fmt::Debug for RuntimeTurn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => formatter.write_str("Idle"),
            Self::Ready { history } => formatter
                .debug_struct("Ready")
                .field("history", history)
                .finish(),
            Self::Running {
                abort_requested, ..
            } => formatter
                .debug_struct("Running")
                .field("abort_requested", abort_requested)
                .finish(),
            Self::PendingAck { history } => formatter
                .debug_struct("PendingAck")
                .field("history", history)
                .finish(),
            Self::Failed { history } => formatter
                .debug_struct("Failed")
                .field("history", history)
                .finish(),
        }
    }
}

#[derive(Clone)]
pub struct AgentRuntime {
    state: Arc<Mutex<RuntimeState>>,
    config: AgentTurnConfig,
    model: Arc<dyn Model>,
    tools: Arc<dyn ToolRouter>,
}

impl fmt::Debug for AgentRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock().map_err(|_| fmt::Error)?;
        f.debug_struct("AgentRuntime")
            .field("turn", &state.turn)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq)]
// RuntimeEvent is an in-process UI channel. Keeping protocol events unboxed
// preserves direct protocol matching at the TUI boundary.
#[allow(clippy::large_enum_variant)]
pub enum RuntimeEvent {
    Agent(AgentEvent),
    TurnFinished {
        history: AgentHistory,
    },
    /// `history` is the authoritative runtime history after rollback. `Agent`
    /// events are forwarded only if produced by the agent framework, and are
    /// not synthesized or retracted by the runtime.
    TurnFailed {
        history: AgentHistory,
        message: String,
    },
    ControlFailed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTurnSnapshot {
    Idle,
    Running,
    Paused,
    Preempting,
    PendingAck,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeBusy;

impl fmt::Display for RuntimeBusy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("runtime is busy")
    }
}

impl StdError for RuntimeBusy {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeControlError {
    NoActiveTurn,
    Agent(String),
}

impl fmt::Display for RuntimeControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoActiveTurn => formatter.write_str("no active turn"),
            Self::Agent(message) => formatter.write_str(message),
        }
    }
}

impl StdError for RuntimeControlError {}

impl AgentRuntime {
    pub fn new(config: AgentTurnConfig, model: Arc<dyn Model>, tools: Arc<dyn ToolRouter>) -> Self {
        Self::new_with_history(config, model, tools, AgentHistory::default())
    }

    pub fn new_with_history(
        config: AgentTurnConfig,
        model: Arc<dyn Model>,
        tools: Arc<dyn ToolRouter>,
        history: AgentHistory,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(RuntimeState {
                turn: RuntimeTurn::Ready { history },
            })),
            config,
            model,
            tools,
        }
    }

    pub fn is_busy(&self) -> bool {
        !matches!(
            self.state
                .lock()
                .expect("runtime state mutex poisoned")
                .turn,
            RuntimeTurn::Idle | RuntimeTurn::Ready { .. }
        )
    }

    pub fn history(&self) -> Option<AgentHistory> {
        match &self
            .state
            .lock()
            .expect("runtime state mutex poisoned")
            .turn
        {
            RuntimeTurn::Ready { history }
            | RuntimeTurn::PendingAck { history }
            | RuntimeTurn::Failed { history } => Some(history.clone()),
            RuntimeTurn::Idle | RuntimeTurn::Running { .. } => None,
        }
    }

    pub fn config(&self) -> AgentTurnConfig {
        self.config.clone()
    }

    pub fn turn_snapshot(&self) -> RuntimeTurnSnapshot {
        let state = self.state.lock().expect("runtime state mutex poisoned");
        match &state.turn {
            RuntimeTurn::Idle | RuntimeTurn::Ready { .. } => RuntimeTurnSnapshot::Idle,
            RuntimeTurn::Running { .. } => RuntimeTurnSnapshot::Running,
            RuntimeTurn::PendingAck { .. } => RuntimeTurnSnapshot::PendingAck,
            RuntimeTurn::Failed { .. } => RuntimeTurnSnapshot::Failed,
        }
    }

    pub fn reset_history(&mut self, seed: AgentHistory) -> Result<(), RuntimeBusy> {
        let mut state = self.state.lock().expect("runtime state mutex poisoned");
        if matches!(state.turn, RuntimeTurn::Idle | RuntimeTurn::Ready { .. }) {
            state.turn = RuntimeTurn::Ready { history: seed };
            Ok(())
        } else {
            Err(RuntimeBusy)
        }
    }

    pub fn set_config(&mut self, config: AgentTurnConfig) {
        self.config = config;
    }

    pub fn set_model(&mut self, model: Arc<dyn Model>) {
        self.model = model;
    }

    /// Starts an agent turn in a background task on the current Tokio runtime.
    ///
    /// This method calls [`tokio::spawn`] and must be called from within an
    /// active Tokio runtime.
    pub fn submit(
        &mut self,
        input: InputMessage,
        events: mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<(), SubmitError> {
        let mut state = self.state.lock().expect("runtime state mutex poisoned");
        let RuntimeTurn::Ready { history } = &state.turn else {
            return Err(SubmitError::Busy);
        };

        let mut history = history.clone();
        let rollback = history.clone();
        history.push_input(input);
        let config = self.config.clone();
        let model = Arc::clone(&self.model);
        let tools = Arc::clone(&self.tools);
        let turn = start_turn(history, config, model, tools);
        state.turn = RuntimeTurn::Running {
            handle: turn.clone(),
            abort_requested: false,
        };
        drop(state);

        let state = Arc::clone(&self.state);

        tokio::spawn(async move {
            drive_turn(turn, rollback, events, state).await;
        });

        Ok(())
    }

    pub fn finish_success(&mut self, history: AgentHistory) {
        complete_success(&self.state, history);
    }

    pub fn finish_failure(&mut self, history: AgentHistory) {
        complete_failure(&self.state, history);
    }

    pub async fn abort_active(&self) -> Result<(), RuntimeControlError> {
        let handle = self.active_handle(true)?;
        if matches!(handle.state().await, Ok(AgentTurnState::Paused { .. })) {
            handle
                .resume()
                .await
                .map_err(|error| RuntimeControlError::Agent(error.to_string()))?;
        }
        handle
            .preempt()
            .await
            .map_err(|error| RuntimeControlError::Agent(error.to_string()))
    }

    pub async fn pause_active(&self) -> Result<(), RuntimeControlError> {
        self.active_handle(false)?
            .pause()
            .await
            .map_err(|error| RuntimeControlError::Agent(error.to_string()))
    }

    pub async fn resume_active(&self) -> Result<(), RuntimeControlError> {
        let handle = self.active_handle(false)?;
        match handle.state().await {
            Ok(AgentTurnState::Paused { .. }) => {}
            Ok(_) => {
                return Err(RuntimeControlError::Agent("turn is not paused".to_string()));
            }
            Err(error) => return Err(RuntimeControlError::Agent(error.to_string())),
        }

        handle
            .resume()
            .await
            .map_err(|error| RuntimeControlError::Agent(error.to_string()))
    }

    fn active_handle(&self, mark_abort: bool) -> Result<AgentTurnHandle, RuntimeControlError> {
        let mut state = self.state.lock().expect("runtime state mutex poisoned");
        match &mut state.turn {
            RuntimeTurn::Running {
                handle,
                abort_requested,
            } => {
                if mark_abort {
                    *abort_requested = true;
                }
                Ok(handle.clone())
            }
            RuntimeTurn::Idle
            | RuntimeTurn::Ready { .. }
            | RuntimeTurn::PendingAck { .. }
            | RuntimeTurn::Failed { .. } => Err(RuntimeControlError::NoActiveTurn),
        }
    }
}

async fn drive_turn(
    turn: AgentTurnHandle,
    rollback: AgentHistory,
    events: mpsc::UnboundedSender<RuntimeEvent>,
    state: Arc<Mutex<RuntimeState>>,
) {
    loop {
        let point = match turn.step_until_control().await {
            Ok(point) => point,
            Err(error) => {
                if abort_requested(&state) {
                    complete_intentional_abort(turn, &state, &events).await;
                } else {
                    fail_turn(&state, &events, &rollback, error.to_string());
                }
                return;
            }
        };

        for event in point.events().iter().cloned() {
            let _ = events.send(RuntimeEvent::Agent(event));
        }

        let finished = matches!(point.pending_outcome(), AgentOutcome::TurnFinished);
        if let Err(error) = point.continue_turn().await {
            if abort_requested(&state) {
                complete_intentional_abort(turn, &state, &events).await;
            } else {
                fail_turn(&state, &events, &rollback, error.to_string());
            }
            return;
        }

        if finished {
            let mut stream = turn.clone().events();
            while let Some(event) = stream.next().await {
                match event {
                    Ok(event) => {
                        let _ = events.send(RuntimeEvent::Agent(event));
                    }
                    Err(error) => {
                        fail_turn(&state, &events, &rollback, error.to_string());
                        return;
                    }
                }
            }

            let history = turn.into_history().await;
            complete_pending_ack(&state, history.clone());
            if events
                .send(RuntimeEvent::TurnFinished {
                    history: history.clone(),
                })
                .is_err()
            {
                complete_success(&state, history);
            }
            return;
        }
    }
}

async fn complete_intentional_abort(
    turn: AgentTurnHandle,
    state: &Arc<Mutex<RuntimeState>>,
    events: &mpsc::UnboundedSender<RuntimeEvent>,
) {
    let mut stream = turn.clone().events();
    while let Some(event) = stream.next().await {
        if let Ok(event) = event {
            let _ = events.send(RuntimeEvent::Agent(event));
        }
    }

    let history = turn.into_history().await;
    complete_pending_ack(state, history.clone());
    if events
        .send(RuntimeEvent::TurnFinished {
            history: history.clone(),
        })
        .is_err()
    {
        complete_success(state, history);
    }
}

fn abort_requested(state: &Arc<Mutex<RuntimeState>>) -> bool {
    let state = state.lock().expect("runtime state mutex poisoned");
    matches!(
        state.turn,
        RuntimeTurn::Running {
            abort_requested: true,
            ..
        }
    )
}

fn fail_turn(
    state: &Arc<Mutex<RuntimeState>>,
    events: &mpsc::UnboundedSender<RuntimeEvent>,
    history: &AgentHistory,
    message: String,
) {
    let history = history.clone();
    complete_pending_failure(state, history.clone());
    if events
        .send(RuntimeEvent::TurnFailed {
            history: history.clone(),
            message,
        })
        .is_err()
    {
        complete_failure(state, history);
    }
}

fn complete_pending_ack(state: &Arc<Mutex<RuntimeState>>, history: AgentHistory) {
    let mut state = state.lock().expect("runtime state mutex poisoned");
    state.turn = RuntimeTurn::PendingAck { history };
}

fn complete_pending_failure(state: &Arc<Mutex<RuntimeState>>, history: AgentHistory) {
    let mut state = state.lock().expect("runtime state mutex poisoned");
    state.turn = RuntimeTurn::Failed { history };
}

fn complete_success(state: &Arc<Mutex<RuntimeState>>, history: AgentHistory) {
    let mut state = state.lock().expect("runtime state mutex poisoned");
    state.turn = RuntimeTurn::Ready { history };
}

fn complete_failure(state: &Arc<Mutex<RuntimeState>>, history: AgentHistory) {
    let mut state = state.lock().expect("runtime state mutex poisoned");
    state.turn = RuntimeTurn::Ready { history };
}

#[cfg(test)]
mod tests {
    use super::{AgentRuntime, RuntimeEvent, SubmitError};
    use copro_agent::{AgentEvent, AgentTurnConfig, ToolExecutionPolicy, ToolRouter, async_trait};
    use copro_api::error::{Error, Result};
    use copro_api::message::{
        InputContent, InputMessage, Message, OutputContent, ToolCall, ToolResult,
    };
    use copro_api::request::GenerateRequest;
    use copro_api::response::FinishReason;
    use copro_api::stream::{Model, ModelStream, OutputContentDelta, OutputStreamEvent};
    use copro_api::tool::ToolDefinition;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    struct TextModel;

    impl Model for TextModel {
        fn stream(&self, _request: GenerateRequest) -> ModelStream {
            Box::pin(futures_util::stream::iter(vec![
                Ok(OutputStreamEvent::Delta {
                    content_index: 0,
                    delta: OutputContentDelta::Text("hello".to_string()),
                }),
                Ok(OutputStreamEvent::Finished {
                    reason: FinishReason::Stop,
                    usage: None,
                }),
            ]))
        }
    }

    struct FailingModel;

    impl Model for FailingModel {
        fn stream(&self, _request: GenerateRequest) -> ModelStream {
            Box::pin(futures_util::stream::iter(vec![Err(Error::client(
                "missing api key",
            ))]))
        }
    }

    struct DeltaThenFailModel;

    impl Model for DeltaThenFailModel {
        fn stream(&self, _request: GenerateRequest) -> ModelStream {
            Box::pin(futures_util::stream::iter(vec![
                Ok(OutputStreamEvent::Delta {
                    content_index: 0,
                    delta: OutputContentDelta::Text("partial".to_string()),
                }),
                Err(Error::client("stream failed")),
            ]))
        }
    }

    struct PendingModel;

    impl Model for PendingModel {
        fn stream(&self, _request: GenerateRequest) -> ModelStream {
            Box::pin(futures_util::stream::pending())
        }
    }

    #[derive(Default)]
    struct NoopTools;

    #[async_trait]
    impl ToolRouter for NoopTools {
        async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
            Ok(Vec::new())
        }

        async fn execute(
            &self,
            call: ToolCall,
            _cancel: copro_agent::CancellationToken,
        ) -> Result<ToolResult> {
            Err(Error::client(format!("unknown tool: {}", call.name)))
        }

        async fn execution_policy(&self, _call: &ToolCall) -> Result<ToolExecutionPolicy> {
            Ok(ToolExecutionPolicy::Serial)
        }
    }

    fn runtime(model: impl Model + 'static) -> AgentRuntime {
        AgentRuntime::new(
            AgentTurnConfig::default(),
            Arc::new(model),
            Arc::new(NoopTools),
        )
    }

    fn user_message(text: &str) -> InputMessage {
        InputMessage::User(vec![InputContent::Text(text.to_string())])
    }

    fn runtime_messages(runtime: &AgentRuntime) -> Option<Vec<Message>> {
        runtime.history().map(|history| history.messages().to_vec())
    }

    fn developer_message(text: &str) -> Message {
        Message::developer(vec![InputContent::Text(text.to_string())])
    }

    async fn recv_terminal_event(rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> RuntimeEvent {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("runtime event timed out")
                .expect("runtime event channel closed");
            if matches!(
                event,
                RuntimeEvent::TurnFinished { .. } | RuntimeEvent::TurnFailed { .. }
            ) {
                return event;
            }
        }
    }

    async fn wait_until_not_busy(runtime: &AgentRuntime) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while runtime.is_busy() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("runtime stayed busy");
    }

    #[test]
    fn runtime_can_start_with_initial_history() {
        let history = copro_agent::AgentHistory::from_messages(vec![developer_message(
            "Current workspace: /workspace",
        )]);
        let runtime = AgentRuntime::new_with_history(
            AgentTurnConfig::default(),
            Arc::new(PendingModel),
            Arc::new(NoopTools),
            history.clone(),
        );

        assert_eq!(runtime.history(), Some(history));
    }

    #[tokio::test]
    async fn successful_submit_finishes_with_updated_history() {
        let mut runtime = runtime(TextModel);
        let (tx, mut rx) = mpsc::unbounded_channel();

        runtime.submit(user_message("hi"), tx).unwrap();

        let event = recv_terminal_event(&mut rx).await;
        let RuntimeEvent::TurnFinished { history } = event else {
            panic!("expected finished event");
        };

        assert_eq!(
            history.messages(),
            &[
                Message::user(vec![InputContent::Text("hi".to_string())]),
                Message::assistant(vec![OutputContent::Text("hello".to_string())]),
            ]
        );

        runtime.finish_success(history);

        assert!(!runtime.is_busy());
        assert_eq!(
            runtime_messages(&runtime),
            Some(vec![
                Message::user(vec![InputContent::Text("hi".to_string())]),
                Message::assistant(vec![OutputContent::Text("hello".to_string())]),
            ])
        );
    }

    #[tokio::test]
    async fn completed_submit_remains_busy_until_terminal_event_is_acked() {
        let mut runtime = runtime(TextModel);
        let (tx, mut rx) = mpsc::unbounded_channel();

        runtime.submit(user_message("first"), tx.clone()).unwrap();

        let event = recv_terminal_event(&mut rx).await;
        let RuntimeEvent::TurnFinished { history } = event else {
            panic!("expected finished event");
        };

        assert_eq!(
            runtime.submit(user_message("second"), tx),
            Err(SubmitError::Busy)
        );
        assert!(runtime.is_busy());

        runtime.finish_success(history);

        let (tx, _rx) = mpsc::unbounded_channel();
        assert_eq!(runtime.submit(user_message("second"), tx), Ok(()));
    }

    #[tokio::test]
    async fn successful_submit_recovers_state_when_receiver_is_dropped() {
        let mut runtime = runtime(TextModel);
        let (tx, rx) = mpsc::unbounded_channel();

        runtime.submit(user_message("hi"), tx).unwrap();
        drop(rx);

        wait_until_not_busy(&runtime).await;

        assert_eq!(
            runtime_messages(&runtime),
            Some(vec![
                Message::user(vec![InputContent::Text("hi".to_string())]),
                Message::assistant(vec![OutputContent::Text("hello".to_string())]),
            ])
        );

        let (tx, _rx) = mpsc::unbounded_channel();
        assert_eq!(runtime.submit(user_message("again"), tx), Ok(()));
    }

    #[tokio::test]
    async fn failed_submit_rolls_back_history() {
        let mut runtime = runtime(FailingModel);
        let (tx, mut rx) = mpsc::unbounded_channel();

        runtime.submit(user_message("hi"), tx).unwrap();

        let event = recv_terminal_event(&mut rx).await;
        let RuntimeEvent::TurnFailed { history, message } = event else {
            panic!("expected failed event");
        };

        assert!(message.contains("missing api key"));
        assert!(history.messages().is_empty());

        runtime.finish_failure(history);

        assert!(!runtime.is_busy());
        assert_eq!(runtime_messages(&runtime), Some(Vec::new()));
    }

    #[tokio::test]
    async fn failed_submit_after_partial_event_does_not_synthesize_model_delta() {
        let mut runtime = runtime(DeltaThenFailModel);
        let (tx, mut rx) = mpsc::unbounded_channel();

        runtime.submit(user_message("hi"), tx).unwrap();

        let (history, message) = loop {
            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("runtime event timed out")
                .expect("runtime event channel closed");
            match event {
                RuntimeEvent::Agent(AgentEvent::ModelDelta {
                    delta: OutputContentDelta::Text(text),
                    ..
                }) if text == "partial" => {
                    panic!("runtime must not synthesize uncommitted model delta");
                }
                RuntimeEvent::TurnFailed { history, message } => break (history, message),
                _ => {}
            }
        };

        assert!(message.contains("stream failed"));
        assert!(history.messages().is_empty());
    }

    #[tokio::test]
    async fn busy_submit_does_not_consume_input_or_change_history() {
        let mut runtime = runtime(PendingModel);
        let (tx, _rx) = mpsc::unbounded_channel();

        runtime.submit(user_message("first"), tx.clone()).unwrap();

        assert_eq!(
            runtime.submit(user_message("second"), tx),
            Err(SubmitError::Busy)
        );
        assert!(runtime.is_busy());
        assert!(runtime.history().is_none());
    }
}
