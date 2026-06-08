use copro_agent::{
    AgentEvent, AgentHistory, AgentOutcome, AgentTurnConfig, AgentTurnHandle, AgentTurnState,
    ToolRouter, start_turn,
};
use copro_api::message::InputMessage;
use copro_api::stream::Model;
use futures_util::StreamExt;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::{error::Error as StdError, fmt};
use tokio::sync::mpsc;

struct QueuedInputBatch {
    inputs: Vec<InputMessage>,
    events: mpsc::UnboundedSender<RuntimeEvent>,
}

struct RuntimeState {
    turn: RuntimeTurn,
    queue: VecDeque<QueuedInputBatch>,
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
        pending_steers: VecDeque<InputMessage>,
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
    SteerQueued {
        input: InputMessage,
    },
    QueuedInputSubmitted {
        input: InputMessage,
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
pub enum DeliveryIntent {
    Submit,
    Steer,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryResult {
    Submitted,
    Steered,
    Queued,
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
                queue: VecDeque::new(),
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
            state.queue.clear();
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
    pub fn accept_user_input(
        &mut self,
        input: InputMessage,
        delivery: DeliveryIntent,
        events: mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<DeliveryResult, SubmitError> {
        match delivery {
            DeliveryIntent::Submit => self.start_input_turn(input, events),
            DeliveryIntent::Steer => self.steer_input(input),
            DeliveryIntent::Queue => {
                self.queue_input(input, events);
                Ok(DeliveryResult::Queued)
            }
        }
    }

    fn start_input_turn(
        &mut self,
        input: InputMessage,
        events: mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<DeliveryResult, SubmitError> {
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
            pending_steers: VecDeque::new(),
        };
        drop(state);

        let state = Arc::clone(&self.state);

        tokio::spawn(async move {
            drive_turn(turn, rollback, events, state).await;
        });

        Ok(DeliveryResult::Submitted)
    }

    fn steer_input(&mut self, input: InputMessage) -> Result<DeliveryResult, SubmitError> {
        let mut state = self.state.lock().expect("runtime state mutex poisoned");
        let RuntimeTurn::Running {
            handle,
            pending_steers,
            ..
        } = &mut state.turn
        else {
            return Err(SubmitError::Busy);
        };

        handle.push_input(input.clone());
        pending_steers.push_back(input);
        Ok(DeliveryResult::Steered)
    }

    fn queue_input(&mut self, input: InputMessage, events: mpsc::UnboundedSender<RuntimeEvent>) {
        let mut state = self.state.lock().expect("runtime state mutex poisoned");
        state.queue.push_back(QueuedInputBatch {
            inputs: vec![input],
            events,
        });
    }

    pub fn finish_success(&mut self, history: AgentHistory) {
        if let Some((turn, rollback, batch)) = self.promote_next_queued(history) {
            let QueuedInputBatch { inputs, events } = batch;
            for input in inputs {
                let _ = events.send(RuntimeEvent::QueuedInputSubmitted { input });
            }
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                drive_turn(turn, rollback, events, state).await;
            });
        }
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
                ..
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

    fn promote_next_queued(
        &mut self,
        history: AgentHistory,
    ) -> Option<(AgentTurnHandle, AgentHistory, QueuedInputBatch)> {
        let mut state = self.state.lock().expect("runtime state mutex poisoned");
        let Some(batch) = state.queue.pop_front() else {
            state.turn = RuntimeTurn::Ready { history };
            return None;
        };

        let mut history = history;
        let rollback = history.clone();
        for input in batch.inputs.iter().cloned() {
            history.push_input(input);
        }
        let turn = start_turn(
            history,
            self.config.clone(),
            Arc::clone(&self.model),
            Arc::clone(&self.tools),
        );
        state.turn = RuntimeTurn::Running {
            handle: turn.clone(),
            abort_requested: false,
            pending_steers: VecDeque::new(),
        };

        Some((turn, rollback, batch))
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
                    requeue_uncommitted_steers(&state, &events);
                    fail_turn(&state, &events, &rollback, error.to_string());
                }
                return;
            }
        };

        for event in point.events().iter().cloned() {
            if matches!(event, AgentEvent::InputCommitted { .. }) {
                mark_steer_committed(&state);
            }
            let _ = events.send(RuntimeEvent::Agent(event));
        }

        let finished = matches!(point.pending_outcome(), AgentOutcome::TurnFinished);
        if let Err(error) = point.continue_turn().await {
            if abort_requested(&state) {
                complete_intentional_abort(turn, &state, &events).await;
            } else {
                requeue_uncommitted_steers(&state, &events);
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
                        requeue_uncommitted_steers(&state, &events);
                        fail_turn(&state, &events, &rollback, error.to_string());
                        return;
                    }
                }
            }

            requeue_uncommitted_steers(&state, &events);
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

    requeue_uncommitted_steers(state, events);
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

fn mark_steer_committed(state: &Arc<Mutex<RuntimeState>>) {
    let mut state = state.lock().expect("runtime state mutex poisoned");
    if let RuntimeTurn::Running { pending_steers, .. } = &mut state.turn {
        pending_steers.pop_front();
    }
}

fn requeue_uncommitted_steers(
    state: &Arc<Mutex<RuntimeState>>,
    events: &mpsc::UnboundedSender<RuntimeEvent>,
) {
    let inputs = {
        let mut state = state.lock().expect("runtime state mutex poisoned");
        let inputs = {
            let RuntimeTurn::Running { pending_steers, .. } = &mut state.turn else {
                return;
            };
            pending_steers.drain(..).collect::<Vec<_>>()
        };
        if !inputs.is_empty() {
            state.queue.push_front(QueuedInputBatch {
                inputs: inputs.clone(),
                events: events.clone(),
            });
        }
        inputs
    };

    for input in inputs {
        let _ = events.send(RuntimeEvent::SteerQueued { input });
    }
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
    use super::{AgentRuntime, DeliveryIntent, DeliveryResult, RuntimeEvent, SubmitError};
    use copro_agent::{AgentEvent, AgentTurnConfig, ToolExecutionPolicy, ToolRouter, async_trait};
    use copro_api::error::{Error, Result};
    use copro_api::message::{
        InputContent, InputMessage, Message, OutputContent, ToolCall, ToolResult,
    };
    use copro_api::request::GenerateRequest;
    use copro_api::response::FinishReason;
    use copro_api::stream::{Model, ModelStream, OutputContentDelta, OutputStreamEvent};
    use copro_api::tool::ToolDefinition;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
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

    struct CapturingDoneModel {
        requests: Arc<StdMutex<Vec<GenerateRequest>>>,
    }

    impl Model for CapturingDoneModel {
        fn stream(&self, request: GenerateRequest) -> ModelStream {
            self.requests
                .lock()
                .expect("captured request mutex poisoned")
                .push(request);
            Box::pin(futures_util::stream::iter(vec![Ok(
                OutputStreamEvent::Finished {
                    reason: FinishReason::Stop,
                    usage: None,
                },
            )]))
        }
    }

    struct FirstPendingThenDoneModel {
        requests: Arc<StdMutex<Vec<GenerateRequest>>>,
        streams: AtomicUsize,
    }

    impl Model for FirstPendingThenDoneModel {
        fn stream(&self, request: GenerateRequest) -> ModelStream {
            self.requests
                .lock()
                .expect("captured request mutex poisoned")
                .push(request);
            if self.streams.fetch_add(1, Ordering::SeqCst) == 0 {
                Box::pin(futures_util::stream::pending())
            } else {
                Box::pin(futures_util::stream::iter(vec![Ok(
                    OutputStreamEvent::Finished {
                        reason: FinishReason::Stop,
                        usage: None,
                    },
                )]))
            }
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

    fn deliver(
        runtime: &mut AgentRuntime,
        intent: DeliveryIntent,
        text: &str,
        events: mpsc::UnboundedSender<RuntimeEvent>,
    ) -> std::result::Result<DeliveryResult, SubmitError> {
        runtime.accept_user_input(user_message(text), intent, events)
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

    async fn recv_queued_input(rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> InputMessage {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("runtime event timed out")
                .expect("runtime event channel closed");
            if let RuntimeEvent::QueuedInputSubmitted { input } = event {
                return input;
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

    async fn wait_until_request_count(
        requests: &Arc<StdMutex<Vec<GenerateRequest>>>,
        count: usize,
    ) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if requests
                    .lock()
                    .expect("captured request mutex poisoned")
                    .len()
                    >= count
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("request was not captured");
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

        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "hi", tx),
            Ok(DeliveryResult::Submitted)
        );

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

        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "first", tx.clone()),
            Ok(DeliveryResult::Submitted)
        );

        let event = recv_terminal_event(&mut rx).await;
        let RuntimeEvent::TurnFinished { history } = event else {
            panic!("expected finished event");
        };

        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "second", tx),
            Err(SubmitError::Busy)
        );
        assert!(runtime.is_busy());

        runtime.finish_success(history);

        let (tx, _rx) = mpsc::unbounded_channel();
        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "second", tx),
            Ok(DeliveryResult::Submitted)
        );
    }

    #[tokio::test]
    async fn queued_inputs_start_separate_follow_up_turns() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut runtime = AgentRuntime::new(
            AgentTurnConfig::default(),
            Arc::new(CapturingDoneModel {
                requests: Arc::clone(&requests),
            }),
            Arc::new(NoopTools),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (second_tx, mut second_rx) = mpsc::unbounded_channel();
        let (third_tx, mut third_rx) = mpsc::unbounded_channel();

        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "first", tx.clone()),
            Ok(DeliveryResult::Submitted)
        );
        let RuntimeEvent::TurnFinished { history } = recv_terminal_event(&mut rx).await else {
            panic!("expected first turn finished event");
        };

        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Queue, "second", second_tx),
            Ok(DeliveryResult::Queued)
        );
        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Queue, "third", third_tx),
            Ok(DeliveryResult::Queued)
        );
        runtime.finish_success(history);

        assert_eq!(
            recv_queued_input(&mut second_rx).await,
            user_message("second")
        );
        let RuntimeEvent::TurnFinished { history } = recv_terminal_event(&mut second_rx).await
        else {
            panic!("expected second turn finished event");
        };
        runtime.finish_success(history);

        assert_eq!(
            recv_queued_input(&mut third_rx).await,
            user_message("third")
        );
        let RuntimeEvent::TurnFinished { history } = recv_terminal_event(&mut third_rx).await
        else {
            panic!("expected third turn finished event");
        };
        runtime.finish_success(history);

        assert!(!runtime.is_busy());
        assert_eq!(
            *requests.lock().expect("captured request mutex poisoned"),
            vec![
                GenerateRequest {
                    messages: vec![Message::user(vec![InputContent::Text("first".to_string())])],
                    tools: Vec::new(),
                    tool_choice: None,
                    hosted_tools: Vec::new(),
                    options: Default::default(),
                },
                GenerateRequest {
                    messages: vec![
                        Message::user(vec![InputContent::Text("first".to_string())]),
                        Message::assistant(Vec::new()),
                        Message::user(vec![InputContent::Text("second".to_string())]),
                    ],
                    tools: Vec::new(),
                    tool_choice: None,
                    hosted_tools: Vec::new(),
                    options: Default::default(),
                },
                GenerateRequest {
                    messages: vec![
                        Message::user(vec![InputContent::Text("first".to_string())]),
                        Message::assistant(Vec::new()),
                        Message::user(vec![InputContent::Text("second".to_string())]),
                        Message::assistant(Vec::new()),
                        Message::user(vec![InputContent::Text("third".to_string())]),
                    ],
                    tools: Vec::new(),
                    tool_choice: None,
                    hosted_tools: Vec::new(),
                    options: Default::default(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn uncommitted_steers_fallback_as_one_follow_up_turn() {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let mut runtime = AgentRuntime::new(
            AgentTurnConfig::default(),
            Arc::new(FirstPendingThenDoneModel {
                requests: Arc::clone(&requests),
                streams: AtomicUsize::new(0),
            }),
            Arc::new(NoopTools),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();

        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "first", tx.clone()),
            Ok(DeliveryResult::Submitted)
        );
        wait_until_request_count(&requests, 1).await;
        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Steer, "second", tx.clone()),
            Ok(DeliveryResult::Steered)
        );
        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Steer, "third", tx.clone()),
            Ok(DeliveryResult::Steered)
        );
        runtime.abort_active().await.unwrap();

        let RuntimeEvent::TurnFinished { history } = recv_terminal_event(&mut rx).await else {
            panic!("expected aborted first turn to finish");
        };
        runtime.finish_success(history);

        let RuntimeEvent::TurnFinished { history } = recv_terminal_event(&mut rx).await else {
            panic!("expected fallback turn to finish");
        };
        runtime.finish_success(history);

        assert!(!runtime.is_busy());
        assert_eq!(
            *requests.lock().expect("captured request mutex poisoned"),
            vec![
                GenerateRequest {
                    messages: vec![Message::user(vec![InputContent::Text("first".to_string())])],
                    tools: Vec::new(),
                    tool_choice: None,
                    hosted_tools: Vec::new(),
                    options: Default::default(),
                },
                GenerateRequest {
                    messages: vec![
                        Message::user(vec![InputContent::Text("first".to_string())]),
                        Message::user(vec![InputContent::Text("second".to_string())]),
                        Message::user(vec![InputContent::Text("third".to_string())]),
                    ],
                    tools: Vec::new(),
                    tool_choice: None,
                    hosted_tools: Vec::new(),
                    options: Default::default(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn successful_submit_recovers_state_when_receiver_is_dropped() {
        let mut runtime = runtime(TextModel);
        let (tx, rx) = mpsc::unbounded_channel();

        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "hi", tx),
            Ok(DeliveryResult::Submitted)
        );
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
        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "again", tx),
            Ok(DeliveryResult::Submitted)
        );
    }

    #[tokio::test]
    async fn failed_submit_rolls_back_history() {
        let mut runtime = runtime(FailingModel);
        let (tx, mut rx) = mpsc::unbounded_channel();

        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "hi", tx),
            Ok(DeliveryResult::Submitted)
        );

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

        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "hi", tx),
            Ok(DeliveryResult::Submitted)
        );

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

        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "first", tx.clone()),
            Ok(DeliveryResult::Submitted)
        );

        assert_eq!(
            deliver(&mut runtime, DeliveryIntent::Submit, "second", tx),
            Err(SubmitError::Busy)
        );
        assert!(runtime.is_busy());
        assert!(runtime.history().is_none());
    }
}
