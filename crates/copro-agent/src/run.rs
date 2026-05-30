use crate::context::{AgentContext, AgentStreamItem};
use crate::event::{AgentEvent, AgentStream};
use crate::runtime::StopSignal;
use crate::tools::{ToolExecutionPolicy, ToolRouter};
use crate::turn::{
    AgentTurn, AgentTurnPhase, ToolPlanItem, aborted_tool_result, normalize_for_history,
    rejected_tool_result,
};
use copro_api::error::{Error, Result};
use copro_api::message::{
    InputContent, Message, OutputContent, ToolCall, ToolResult, ToolResultStatus,
};
use copro_api::request::GenerateRequest;
use copro_api::response::{FinishReason, Usage};
use copro_api::stream::{ModelStream, OutputContentDelta, OutputStreamEvent};
use copro_api::tool::ToolDefinition;
use derive_more::{Deref, Display, From, Into};
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::any::Any;
use std::mem;
use std::result::Result as StdResult;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deref, Display, From, Into)]
pub struct AgentRunId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deref, Display, From, Into)]
pub struct AgentTurnId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentStepId {
    pub run_id: AgentRunId,
    pub turn_id: AgentTurnId,
    pub tick: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentStep {
    pub id: AgentStepId,
    pub action: AgentAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentAction {
    LoadTools,
    BuildRequest {
        tools: Vec<ToolDefinition>,
    },
    OpenModelStream {
        request: GenerateRequest,
    },
    ReadModelStream,
    CommitAssistant {
        content: Vec<OutputContent>,
        reason: FinishReason,
        usage: Option<Usage>,
    },
    PlanTool {
        tool: ToolCall,
    },
    StartTool {
        tool: ToolCall,
        policy: ToolExecutionPolicy,
    },
    ReadTool {
        tool: ToolCall,
    },
    CommitToolResult {
        tool: ToolCall,
        result: ToolResult,
    },
    FinishTurn,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentOutcome {
    ToolsLoaded(Vec<ToolDefinition>),
    RequestBuilt(GenerateRequest),
    ModelStreamOpened,
    ModelDelta {
        content_index: usize,
        delta: OutputContentDelta,
    },
    ModelDeltaDropped {
        content_index: usize,
        delta: OutputContentDelta,
    },
    ModelOutputFinished {
        content: Vec<OutputContent>,
        reason: FinishReason,
        usage: Option<Usage>,
    },
    AssistantCommitted {
        message_index: usize,
        content: Vec<OutputContent>,
        reason: FinishReason,
        usage: Option<Usage>,
    },
    ToolPlanned {
        tool: ToolCall,
        policy: ToolExecutionPolicy,
    },
    ToolRejected {
        tool: ToolCall,
        result: ToolResult,
    },
    ToolStarted {
        tool: ToolCall,
    },
    ToolFinished {
        tool: ToolCall,
        result: ToolResult,
    },
    ToolResultCommitted {
        message_index: usize,
        tool: ToolCall,
        result: ToolResult,
    },
    TurnFinished,
    ActionInterrupted {
        reason: AgentInterruptReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentInterruptReason {
    Preempted,
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentControl {
    Continue,
    Pause,
    AbortTurn,
    AbortRun,
    ReplaceRequest(GenerateRequest),
    ReplaceModelDelta(OutputContentDelta),
    DropModelDelta,
    ReplaceAssistantOutput(Vec<OutputContent>),
    ReplaceToolCall(ToolCall),
    RejectToolCall { reason: String },
    ReplaceToolResult(ToolResult),
}

impl AgentControl {
    pub fn kind(&self) -> AgentControlKind {
        match self {
            AgentControl::Continue => AgentControlKind::Continue,
            AgentControl::Pause => AgentControlKind::Pause,
            AgentControl::AbortTurn => AgentControlKind::AbortTurn,
            AgentControl::AbortRun => AgentControlKind::AbortRun,
            AgentControl::ReplaceRequest(_) => AgentControlKind::ReplaceRequest,
            AgentControl::ReplaceModelDelta(_) => AgentControlKind::ReplaceModelDelta,
            AgentControl::DropModelDelta => AgentControlKind::DropModelDelta,
            AgentControl::ReplaceAssistantOutput(_) => AgentControlKind::ReplaceAssistantOutput,
            AgentControl::ReplaceToolCall(_) => AgentControlKind::ReplaceToolCall,
            AgentControl::RejectToolCall { .. } => AgentControlKind::RejectToolCall,
            AgentControl::ReplaceToolResult(_) => AgentControlKind::ReplaceToolResult,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentControlKind {
    Continue,
    Pause,
    AbortTurn,
    AbortRun,
    ReplaceRequest,
    ReplaceModelDelta,
    DropModelDelta,
    ReplaceAssistantOutput,
    ReplaceToolCall,
    RejectToolCall,
    ReplaceToolResult,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentRunState {
    Ready {
        next: AgentAction,
        step_id: AgentStepId,
    },
    InFlight {
        step: AgentStep,
    },
    WaitingControl {
        step: AgentStep,
        outcome: AgentOutcome,
    },
    Paused {
        at: AgentStepId,
    },
    Preempting {
        step_id: AgentStepId,
    },
    Recovering {
        after: AgentStepId,
    },
    Finished,
    Aborted,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentStepReport {
    pub step: AgentStep,
    pub outcome: AgentOutcome,
    pub state: AgentRunState,
    pub events: Vec<AgentEvent>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentControlPoint {
    pub step: AgentStep,
    pub pending_outcome: AgentOutcome,
    pub allowed_controls: Vec<AgentControlKind>,
}

#[derive(Clone)]
pub struct AgentRunHandle {
    events: Arc<Mutex<mpsc::Receiver<AgentStreamItem>>>,
    state: Arc<Mutex<AgentRunHandleState>>,
    stop_signal: StopSignal,
}

struct AgentRunHandleState {
    pending_control: Option<(AgentStepId, oneshot::Sender<AgentControl>)>,
    last_report: Option<AgentStepReport>,
    state: Option<AgentRunState>,
}

impl AgentRunHandle {
    pub(crate) fn new(events: mpsc::Receiver<AgentStreamItem>, stop_signal: StopSignal) -> Self {
        Self {
            events: Arc::new(Mutex::new(events)),
            state: Arc::new(Mutex::new(AgentRunHandleState {
                pending_control: None,
                last_report: None,
                state: None,
            })),
            stop_signal,
        }
    }

    pub fn into_stream(self) -> AgentStream {
        Box::pin(async_stream::try_stream! {
            while let Some(event) = self.next_event().await? {
                yield event;
            }
        })
    }

    pub fn events(self) -> AgentStream {
        self.into_stream()
    }

    pub async fn step(&self) -> Result<AgentStepReport> {
        self.state.lock().await.continue_pending_control();

        let mut events = Vec::new();
        loop {
            let item = self
                .events
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| Error::client("agent run finished"))?;

            match item {
                AgentStreamItem::Event(event, ack) => {
                    let event = *event;
                    events.push(event.clone());
                    if let AgentEvent::ControlRequired { step, outcome } = event {
                        let mut state_inner = self.state.lock().await;
                        let state = AgentRunState::WaitingControl {
                            step: step.clone(),
                            outcome: outcome.clone(),
                        };
                        state_inner.state = Some(state.clone());
                        state_inner.pending_control = Some((step.id, ack));
                        let report = AgentStepReport {
                            step,
                            outcome,
                            state,
                            events,
                        };
                        state_inner.last_report = Some(report.clone());
                        return Ok(report);
                    }
                    self.state.lock().await.observe_event(&event);
                    let _ = ack.send(AgentControl::Continue);
                }
                AgentStreamItem::Error(error) => {
                    self.state.lock().await.state = Some(AgentRunState::Aborted);
                    return Err(error);
                }
            }
        }
    }

    pub async fn step_until_control(&self) -> Result<AgentControlPoint> {
        let report = self.step().await?;
        Ok(AgentControlPoint {
            step: report.step,
            allowed_controls: allowed_controls_for_outcome(&report.outcome),
            pending_outcome: report.outcome,
        })
    }

    pub async fn control(
        &self,
        step_id: AgentStepId,
        control: AgentControl,
    ) -> Result<AgentStepReport> {
        let mut inner = self.state.lock().await;
        let report = inner
            .last_report
            .clone()
            .ok_or_else(|| Error::client("agent run is not waiting for control"))?;
        let Some((pending_step_id, _)) = &inner.pending_control else {
            return Err(Error::client("agent run is not waiting for control"));
        };
        if *pending_step_id != step_id {
            return Err(Error::client("stale agent control step id"));
        }

        match control {
            AgentControl::Continue => {
                inner.send_pending_control(AgentControl::Continue);
                Ok(report)
            }
            AgentControl::Pause => {
                inner.state = Some(AgentRunState::Paused { at: step_id });
                Ok(report)
            }
            AgentControl::AbortTurn | AgentControl::AbortRun => {
                self.stop_signal.request_stop();
                inner.send_pending_control(control);
                inner.state = Some(AgentRunState::Aborted);
                Ok(report)
            }
            other => {
                inner.send_pending_control(other);
                Ok(report)
            }
        }
    }

    pub async fn pause(&self) -> Result<()> {
        let mut inner = self.state.lock().await;
        let Some((step_id, _)) = &inner.pending_control else {
            return Err(Error::client("agent run is not at a step boundary"));
        };
        inner.state = Some(AgentRunState::Paused { at: *step_id });
        Ok(())
    }

    pub async fn resume(&self) -> Result<()> {
        let mut inner = self.state.lock().await;
        inner.continue_pending_control();
        Ok(())
    }

    pub async fn preempt(&self) -> Result<()> {
        self.stop_signal.request_stop();
        let mut inner = self.state.lock().await;
        if let Some((step_id, _)) = &inner.pending_control {
            inner.state = Some(AgentRunState::Preempting { step_id: *step_id });
        }
        inner.send_pending_control(AgentControl::AbortRun);
        Ok(())
    }

    pub async fn state(&self) -> Result<AgentRunState> {
        let inner = self.state.lock().await;
        inner
            .state
            .clone()
            .ok_or_else(|| Error::client("agent run has not started"))
    }

    async fn next_event(&self) -> Result<Option<AgentEvent>> {
        self.state.lock().await.continue_pending_control();

        match self.events.lock().await.recv().await {
            Some(AgentStreamItem::Event(event, ack)) => {
                let event = *event;
                self.state.lock().await.observe_event(&event);
                let _ = ack.send(AgentControl::Continue);
                Ok(Some(event))
            }
            Some(AgentStreamItem::Error(error)) => {
                self.state.lock().await.state = Some(AgentRunState::Aborted);
                Err(error)
            }
            None => Ok(None),
        }
    }
}

impl AgentRunHandleState {
    fn continue_pending_control(&mut self) {
        self.send_pending_control(AgentControl::Continue);
    }

    fn send_pending_control(&mut self, control: AgentControl) {
        if let Some((_, ack)) = self.pending_control.take() {
            let _ = ack.send(control);
        }
    }

    fn observe_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::StepStarted { step } => {
                self.state = Some(AgentRunState::InFlight { step: step.clone() });
            }
            AgentEvent::ControlRequired { step, outcome } => {
                self.state = Some(AgentRunState::WaitingControl {
                    step: step.clone(),
                    outcome: outcome.clone(),
                });
            }
            AgentEvent::RunPaused { at, .. } => {
                self.state = Some(AgentRunState::Paused { at: *at });
            }
            AgentEvent::RunPreempted { at, .. } => {
                self.state = Some(AgentRunState::Preempting { step_id: *at });
            }
            AgentEvent::RunFinished { .. } => {
                self.state = Some(AgentRunState::Finished);
            }
            AgentEvent::RunAborted { .. } => {
                self.state = Some(AgentRunState::Aborted);
            }
            _ => {}
        }
    }
}

fn allowed_controls_for_outcome(outcome: &AgentOutcome) -> Vec<AgentControlKind> {
    let mut controls = vec![
        AgentControlKind::Continue,
        AgentControlKind::Pause,
        AgentControlKind::AbortTurn,
        AgentControlKind::AbortRun,
    ];
    match outcome {
        AgentOutcome::RequestBuilt(_) => controls.push(AgentControlKind::ReplaceRequest),
        AgentOutcome::ModelDelta { .. } => {
            controls.push(AgentControlKind::ReplaceModelDelta);
            controls.push(AgentControlKind::DropModelDelta);
        }
        AgentOutcome::ModelOutputFinished { .. } => {
            controls.push(AgentControlKind::ReplaceAssistantOutput);
        }
        AgentOutcome::ToolPlanned { .. } | AgentOutcome::ToolRejected { .. } => {
            controls.push(AgentControlKind::ReplaceToolCall);
            controls.push(AgentControlKind::RejectToolCall);
        }
        AgentOutcome::ToolResultCommitted { .. } => {
            controls.push(AgentControlKind::ReplaceToolResult);
        }
        _ => {}
    }
    controls
}

struct AgentRunClock {
    run_id: AgentRunId,
    turn_id: AgentTurnId,
    next_tick: u64,
}

impl AgentRunClock {
    fn new(run_id: AgentRunId, turn_id: AgentTurnId) -> Self {
        Self {
            run_id,
            turn_id,
            next_tick: 0,
        }
    }

    fn next_step(&mut self, action: AgentAction) -> AgentStep {
        let step = AgentStep {
            id: AgentStepId {
                run_id: self.run_id,
                turn_id: self.turn_id,
                tick: self.next_tick,
            },
            action,
        };
        self.next_tick += 1;
        step
    }
}

pub(crate) struct AgentRun<'a> {
    context: &'a mut AgentContext,
}

impl<'a> AgentRun<'a> {
    pub(crate) fn new(context: &'a mut AgentContext) -> Self {
        Self { context }
    }

    pub(crate) async fn run_turn(&mut self, events: mpsc::Sender<AgentStreamItem>) {
        if let Err(error) = self.run_turn_inner(&events).await {
            let _ = events.send(AgentStreamItem::Error(error)).await;
        }
    }

    async fn run_turn_inner(&mut self, events: &mpsc::Sender<AgentStreamItem>) -> Result<()> {
        let model = Arc::clone(&self.context.model);
        let tools = Arc::clone(&self.context.tools);
        let stop_signal = self.context.stop_signal.clone();
        let tool_choice = self.context.tool_choice.clone();
        let hosted_tools = self.context.hosted_tools.clone();
        let options = self.context.options.clone();
        let mut model_stream: Option<ModelStream> = None;
        let (run_id, turn_id) = self.context.allocate_run_ids();
        let mut clock = AgentRunClock::new(run_id, turn_id);

        if !emit(events, AgentEvent::RunStarted { run_id }).await {
            return Ok(());
        }
        if !emit(events, AgentEvent::TurnStarted { run_id, turn_id }).await {
            return Ok(());
        }

        let mut turn = AgentTurn::new();

        loop {
            if turn.is_finished() {
                break;
            }
            if stop_signal.is_requested() && !turn.needs_tool_result_commit() {
                turn.finish();
                break;
            }

            match turn.phase() {
                AgentTurnPhase::ModelRequest => {
                    let step = clock.next_step(AgentAction::LoadTools);
                    if !emit_step_started(events, &step).await {
                        return Ok(());
                    }
                    let tool_definitions = tools.definitions().await?;
                    if !emit_step_completed_and_continue(
                        events,
                        run_id,
                        step,
                        AgentOutcome::ToolsLoaded(tool_definitions.clone()),
                    )
                    .await?
                    {
                        return Ok(());
                    }

                    let step = clock.next_step(AgentAction::BuildRequest {
                        tools: tool_definitions.clone(),
                    });
                    if !emit_step_started(events, &step).await {
                        return Ok(());
                    }
                    let mut request = turn.build_request(
                        &self.context.messages,
                        tool_definitions,
                        hosted_tools.clone(),
                        tool_choice.clone(),
                        options.clone(),
                    );
                    let pending_outcome = AgentOutcome::RequestBuilt(request.clone());
                    let Some(control) =
                        emit_control_required(events, step.clone(), pending_outcome.clone()).await
                    else {
                        return Ok(());
                    };
                    match control {
                        AgentControl::Continue => {}
                        AgentControl::ReplaceRequest(replacement) => {
                            request = replacement;
                        }
                        other => {
                            let _ = complete_step_after_control(
                                events,
                                run_id,
                                step,
                                pending_outcome,
                                other,
                            )
                            .await?;
                            return Ok(());
                        }
                    }
                    if !complete_step_after_control(
                        events,
                        run_id,
                        step,
                        AgentOutcome::RequestBuilt(request.clone()),
                        AgentControl::Continue,
                    )
                    .await?
                    {
                        return Ok(());
                    }

                    let step = clock.next_step(AgentAction::OpenModelStream {
                        request: request.clone(),
                    });
                    if !emit_step_started(events, &step).await {
                        return Ok(());
                    }
                    model_stream = Some(model.stream(request));
                    turn.open_model_stream()?;
                    if !emit_step_completed_and_continue(
                        events,
                        run_id,
                        step,
                        AgentOutcome::ModelStreamOpened,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                }
                AgentTurnPhase::ModelStreaming => {
                    let step = clock.next_step(AgentAction::ReadModelStream);
                    if !emit_step_started(events, &step).await {
                        return Ok(());
                    }
                    let cancel = stop_signal.token();
                    let stream = model_stream
                        .as_mut()
                        .ok_or_else(|| Error::protocol("model stream is not open"))?;
                    let event = tokio::select! {
                        _ = cancel.cancelled() => Ok(None),
                        event = stream.next() => match event {
                            Some(Ok(event)) => Ok(Some(event)),
                            Some(Err(error)) => Err(error),
                            None => Err(Error::protocol("stream ended before finished event")),
                        },
                    }?;

                    let Some(event) = event else {
                        if !emit_step_completed_and_continue(
                            events,
                            run_id,
                            step,
                            AgentOutcome::ActionInterrupted {
                                reason: AgentInterruptReason::Stopped,
                            },
                        )
                        .await?
                        {
                            return Ok(());
                        }
                        turn.finish();
                        break;
                    };

                    match event {
                        OutputStreamEvent::Delta {
                            content_index,
                            delta,
                        } => {
                            let pending_outcome = AgentOutcome::ModelDelta {
                                content_index,
                                delta: delta.clone(),
                            };
                            let Some(control) = emit_control_required(
                                events,
                                step.clone(),
                                pending_outcome.clone(),
                            )
                            .await
                            else {
                                return Ok(());
                            };
                            let (outcome, delta) = match control {
                                AgentControl::Continue => (
                                    AgentOutcome::ModelDelta {
                                        content_index,
                                        delta: delta.clone(),
                                    },
                                    Some(delta),
                                ),
                                AgentControl::ReplaceModelDelta(replacement) => (
                                    AgentOutcome::ModelDelta {
                                        content_index,
                                        delta: replacement.clone(),
                                    },
                                    Some(replacement),
                                ),
                                AgentControl::DropModelDelta => (
                                    AgentOutcome::ModelDeltaDropped {
                                        content_index,
                                        delta,
                                    },
                                    None,
                                ),
                                other => {
                                    let _ = complete_step_after_control(
                                        events,
                                        run_id,
                                        step,
                                        pending_outcome,
                                        other,
                                    )
                                    .await?;
                                    return Ok(());
                                }
                            };
                            if !complete_step_after_control(
                                events,
                                run_id,
                                step.clone(),
                                outcome,
                                AgentControl::Continue,
                            )
                            .await?
                            {
                                return Ok(());
                            }
                            let Some(delta) = delta else {
                                continue;
                            };
                            turn.apply_output_delta(content_index, delta.clone())?;
                            if !emit(
                                events,
                                AgentEvent::ModelDelta {
                                    step_id: step.id,
                                    content_index,
                                    delta: delta.clone(),
                                },
                            )
                            .await
                            {
                                return Ok(());
                            }
                        }
                        OutputStreamEvent::Finished { reason, usage } => {
                            let mut output = turn.finish_output(reason, usage)?;
                            let pending_outcome = AgentOutcome::ModelOutputFinished {
                                content: output.content.clone(),
                                reason: output.reason,
                                usage: output.usage.clone(),
                            };
                            let Some(control) = emit_control_required(
                                events,
                                step.clone(),
                                pending_outcome.clone(),
                            )
                            .await
                            else {
                                return Ok(());
                            };
                            match control {
                                AgentControl::Continue => {}
                                AgentControl::ReplaceAssistantOutput(replacement) => {
                                    output.content = replacement;
                                }
                                other => {
                                    let _ = complete_step_after_control(
                                        events,
                                        run_id,
                                        step,
                                        pending_outcome,
                                        other,
                                    )
                                    .await?;
                                    return Ok(());
                                }
                            }
                            if !complete_step_after_control(
                                events,
                                run_id,
                                step,
                                AgentOutcome::ModelOutputFinished {
                                    content: output.content.clone(),
                                    reason: output.reason,
                                    usage: output.usage.clone(),
                                },
                                AgentControl::Continue,
                            )
                            .await?
                            {
                                return Ok(());
                            }

                            let step = clock.next_step(AgentAction::CommitAssistant {
                                content: output.content.clone(),
                                reason: output.reason,
                                usage: output.usage.clone(),
                            });
                            if !emit_step_started(events, &step).await {
                                return Ok(());
                            }
                            let committed = turn.commit_output(output)?;
                            let message_index = self.context.messages.len();
                            self.context
                                .messages
                                .push(normalize_for_history(Message::Assistant(
                                    committed.content.clone(),
                                )));
                            model_stream = None;
                            let outcome = AgentOutcome::AssistantCommitted {
                                message_index,
                                content: committed.content.clone(),
                                reason: committed.reason,
                                usage: committed.usage.clone(),
                            };
                            if !emit(
                                events,
                                AgentEvent::AssistantCommitted {
                                    step_id: step.id,
                                    message_index,
                                    content: committed.content,
                                    reason: committed.reason,
                                    usage: committed.usage,
                                },
                            )
                            .await
                            {
                                return Ok(());
                            }
                            if !emit_step_completed_and_continue(events, run_id, step, outcome)
                                .await?
                            {
                                return Ok(());
                            }
                        }
                    }
                }
                AgentTurnPhase::ToolPlanning => {
                    if stop_signal.is_requested() {
                        turn.abort_pending_tools()?;
                    } else {
                        let mut plan = Vec::new();

                        for tool in turn.tool_calls()? {
                            let policy = tools.execution_policy(&tool).await?;
                            let mut item = ToolPlanItem::pending(tool, policy);

                            let step = clock.next_step(AgentAction::PlanTool {
                                tool: item.tool.clone(),
                            });
                            if !emit_step_started(events, &step).await {
                                return Ok(());
                            }
                            let outcome = match &item.completed_result {
                                Some(result) => AgentOutcome::ToolRejected {
                                    tool: item.tool.clone(),
                                    result: result.clone(),
                                },
                                None => AgentOutcome::ToolPlanned {
                                    tool: item.tool.clone(),
                                    policy: item.policy,
                                },
                            };
                            let pending_outcome = outcome;
                            let Some(control) = emit_control_required(
                                events,
                                step.clone(),
                                pending_outcome.clone(),
                            )
                            .await
                            else {
                                return Ok(());
                            };
                            let final_outcome;
                            match control {
                                AgentControl::Continue => {
                                    final_outcome = match &item.completed_result {
                                        Some(result) => AgentOutcome::ToolRejected {
                                            tool: item.tool.clone(),
                                            result: result.clone(),
                                        },
                                        None => AgentOutcome::ToolPlanned {
                                            tool: item.tool.clone(),
                                            policy: item.policy,
                                        },
                                    };
                                }
                                AgentControl::ReplaceToolCall(replacement) => {
                                    replace_tool_call_in_history(
                                        &mut self.context.messages,
                                        &item.tool,
                                        replacement.clone(),
                                    );
                                    let policy = tools.execution_policy(&replacement).await?;
                                    item = ToolPlanItem::pending(replacement, policy);
                                    final_outcome = AgentOutcome::ToolPlanned {
                                        tool: item.tool.clone(),
                                        policy: item.policy,
                                    };
                                }
                                AgentControl::RejectToolCall { reason } => {
                                    let result = rejected_tool_result(&item.tool, reason);
                                    item = ToolPlanItem::completed(item.tool, result);
                                    final_outcome = AgentOutcome::ToolRejected {
                                        tool: item.tool.clone(),
                                        result: item
                                            .completed_result
                                            .clone()
                                            .expect("rejected tool call must have a result"),
                                    };
                                }
                                other => {
                                    let _ = complete_step_after_control(
                                        events,
                                        run_id,
                                        step,
                                        pending_outcome,
                                        other,
                                    )
                                    .await?;
                                    return Ok(());
                                }
                            }
                            if !complete_step_after_control(
                                events,
                                run_id,
                                step,
                                final_outcome,
                                AgentControl::Continue,
                            )
                            .await?
                            {
                                return Ok(());
                            }

                            plan.push(item);
                        }

                        turn.set_tool_plan(plan)?;
                    }
                }
                AgentTurnPhase::ToolExecution => {
                    if stop_signal.is_requested() {
                        turn.abort_pending_tools()?;
                    } else {
                        let plan = turn.take_tool_plan()?;
                        let mut completed_tools: Vec<(ToolCall, ToolResult)> = Vec::new();
                        let mut parallel_batch: Vec<ToolCall> = Vec::new();

                        for ToolPlanItem {
                            tool,
                            policy,
                            completed_result,
                        } in plan
                        {
                            if let Some(result) = completed_result {
                                for tool in &parallel_batch {
                                    if !emit_tool_started(
                                        events,
                                        &mut clock,
                                        tool.clone(),
                                        ToolExecutionPolicy::Parallel,
                                    )
                                    .await?
                                    {
                                        return Ok(());
                                    }
                                }
                                let finished = Self::execute_parallel_batch(
                                    Arc::clone(&tools),
                                    stop_signal.token(),
                                    &mut parallel_batch,
                                );
                                let finished = finished.await?;
                                for (tool, result) in &finished {
                                    if !emit_tool_finished(
                                        events,
                                        &mut clock,
                                        tool.clone(),
                                        result.clone(),
                                    )
                                    .await?
                                    {
                                        return Ok(());
                                    }
                                }
                                completed_tools.extend(finished);
                                completed_tools.push((tool, result));
                            } else if policy == ToolExecutionPolicy::Parallel {
                                parallel_batch.push(tool);
                            } else {
                                for tool in &parallel_batch {
                                    if !emit_tool_started(
                                        events,
                                        &mut clock,
                                        tool.clone(),
                                        ToolExecutionPolicy::Parallel,
                                    )
                                    .await?
                                    {
                                        return Ok(());
                                    }
                                }
                                let finished = Self::execute_parallel_batch(
                                    Arc::clone(&tools),
                                    stop_signal.token(),
                                    &mut parallel_batch,
                                );
                                let finished = finished.await?;
                                for (tool, result) in &finished {
                                    if !emit_tool_finished(
                                        events,
                                        &mut clock,
                                        tool.clone(),
                                        result.clone(),
                                    )
                                    .await?
                                    {
                                        return Ok(());
                                    }
                                }
                                completed_tools.extend(finished);
                                if !emit_tool_started(events, &mut clock, tool.clone(), policy)
                                    .await?
                                {
                                    return Ok(());
                                }
                                let completed = Self::execute_tool_call_with_router(
                                    Arc::clone(&tools),
                                    tool,
                                    stop_signal.token(),
                                );
                                let completed = completed.await?;
                                if !emit_tool_finished(
                                    events,
                                    &mut clock,
                                    completed.0.clone(),
                                    completed.1.clone(),
                                )
                                .await?
                                {
                                    return Ok(());
                                }
                                completed_tools.push(completed);
                            }
                        }

                        for tool in &parallel_batch {
                            if !emit_tool_started(
                                events,
                                &mut clock,
                                tool.clone(),
                                ToolExecutionPolicy::Parallel,
                            )
                            .await?
                            {
                                return Ok(());
                            }
                        }
                        let finished = Self::execute_parallel_batch(
                            Arc::clone(&tools),
                            stop_signal.token(),
                            &mut parallel_batch,
                        );
                        let finished = finished.await?;
                        for (tool, result) in &finished {
                            if !emit_tool_finished(events, &mut clock, tool.clone(), result.clone())
                                .await?
                            {
                                return Ok(());
                            }
                        }
                        completed_tools.extend(finished);
                        turn.set_completed_tools(completed_tools, stop_signal.is_requested())?;
                    }
                }
                AgentTurnPhase::ToolResultCommit => {
                    let pending = turn.take_tool_results_for_commit()?;
                    for (tool, mut result) in pending.completed_tools {
                        let step = clock.next_step(AgentAction::CommitToolResult {
                            tool: tool.clone(),
                            result: result.clone(),
                        });
                        if !emit_step_started(events, &step).await {
                            return Ok(());
                        }
                        let message_index = self.context.messages.len();
                        let step_id = step.id;
                        let pending_outcome = AgentOutcome::ToolResultCommitted {
                            message_index,
                            tool: tool.clone(),
                            result: result.clone(),
                        };
                        let Some(control) =
                            emit_control_required(events, step.clone(), pending_outcome.clone())
                                .await
                        else {
                            return Ok(());
                        };
                        match control {
                            AgentControl::Continue => {}
                            AgentControl::ReplaceToolResult(replacement) => {
                                result = replacement;
                            }
                            other => {
                                let _ = complete_step_after_control(
                                    events,
                                    run_id,
                                    step,
                                    pending_outcome,
                                    other,
                                )
                                .await?;
                                return Ok(());
                            }
                        }
                        if !complete_step_after_control(
                            events,
                            run_id,
                            step,
                            AgentOutcome::ToolResultCommitted {
                                message_index,
                                tool: tool.clone(),
                                result: result.clone(),
                            },
                            AgentControl::Continue,
                        )
                        .await?
                        {
                            return Ok(());
                        }
                        self.context.messages.push(Message::Tool(result.clone()));
                        if !emit(
                            events,
                            AgentEvent::ToolResultCommitted {
                                step_id,
                                message_index,
                                tool,
                                result,
                            },
                        )
                        .await
                        {
                            return Ok(());
                        }
                    }

                    let finish_turn = pending.finish_after_commit || stop_signal.is_requested();
                    turn.finish_tool_result_commit(finish_turn);
                }
                AgentTurnPhase::Finished => break,
            };
        }

        let step = clock.next_step(AgentAction::FinishTurn);
        if !emit_step_started(events, &step).await {
            return Ok(());
        }
        if !emit_step_completed_and_continue(events, run_id, step, AgentOutcome::TurnFinished)
            .await?
        {
            return Ok(());
        }
        if !emit(events, AgentEvent::TurnFinished { run_id, turn_id }).await {
            return Ok(());
        }
        if !emit(events, AgentEvent::RunFinished { run_id }).await {
            return Ok(());
        }

        Ok(())
    }

    async fn execute_parallel_batch(
        tools: Arc<dyn ToolRouter>,
        cancel: CancellationToken,
        batch: &mut Vec<ToolCall>,
    ) -> Result<Vec<(ToolCall, ToolResult)>> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }

        let calls = mem::take(batch);
        let mut futures: FuturesUnordered<_> = calls
            .into_iter()
            .enumerate()
            .map(|(index, tool)| {
                let tools = Arc::clone(&tools);
                let cancel = cancel.child_token();
                async move {
                    Self::execute_tool_call_with_router(tools, tool, cancel)
                        .await
                        .map(|completed| (index, completed))
                }
            })
            .collect();

        let mut completed = Vec::new();
        loop {
            if futures.is_empty() {
                break;
            }
            tokio::select! {
                _ = cancel.cancelled() => {
                    while let Some(result) = futures.next().await {
                        completed.push(result?);
                    }
                    break;
                }
                result = futures.next() => {
                    if let Some(result) = result {
                        completed.push(result?);
                    }
                }
            }
        }
        completed.sort_unstable_by_key(|(index, _)| *index);
        Ok(completed
            .into_iter()
            .map(|(_, completed)| completed)
            .collect())
    }

    async fn execute_tool_call_with_router(
        tools: Arc<dyn ToolRouter>,
        tool: ToolCall,
        cancel: CancellationToken,
    ) -> Result<(ToolCall, ToolResult)> {
        let tool_cancel = cancel.child_token();
        let tool_for_abort = tool.clone();
        let mut handle = AbortOnDropHandle::new(tokio::spawn(async move {
            tools.execute(tool, tool_cancel).await
        }));
        tokio::select! {
            biased;
            result = &mut handle => join_tool_task_result(tool_for_abort, result),
            _ = cancel.cancelled() => {
                tokio::select! {
                    biased;
                    result = &mut handle => join_tool_task_result(tool_for_abort, result),
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        handle.abort();
                        Ok((tool_for_abort.clone(), aborted_tool_result(&tool_for_abort)))
                    }
                }
            }
        }
    }
}

fn replace_tool_call_in_history(
    messages: &mut [Message],
    original: &ToolCall,
    replacement: ToolCall,
) {
    for message in messages.iter_mut().rev() {
        let Message::Assistant(content) = message else {
            continue;
        };
        for content in content {
            let OutputContent::ToolCall(tool) = content else {
                continue;
            };
            if tool.id == original.id {
                *tool = replacement;
                return;
            }
        }
    }
}

fn join_tool_task_result(
    tool: ToolCall,
    result: StdResult<Result<ToolResult>, JoinError>,
) -> Result<(ToolCall, ToolResult)> {
    match result {
        Ok(Ok(result)) => Ok((tool, result)),
        Ok(Err(error)) => Err(error),
        Err(join_error) if join_error.is_cancelled() => {
            let result = aborted_tool_result(&tool);
            Ok((tool, result))
        }
        Err(join_error) => {
            let result = panicked_tool_result(&tool, join_error);
            Ok((tool, result))
        }
    }
}

fn panicked_tool_result(tool: &ToolCall, join_error: JoinError) -> ToolResult {
    let panic_message = if join_error.is_panic() {
        panic_payload_to_string(join_error.into_panic())
    } else {
        join_error.to_string()
    };

    ToolResult {
        call_id: tool.id.clone(),
        name: tool.name.clone(),
        status: ToolResultStatus::Error,
        content: vec![InputContent::Text(format!(
            "tool task panicked: {panic_message}"
        ))],
    }
}

fn panic_payload_to_string(payload: Box<dyn Any + Send + 'static>) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

async fn emit_step_started(events: &mpsc::Sender<AgentStreamItem>, step: &AgentStep) -> bool {
    emit(events, AgentEvent::StepStarted { step: step.clone() }).await
}

async fn emit_control_required(
    events: &mpsc::Sender<AgentStreamItem>,
    step: AgentStep,
    outcome: AgentOutcome,
) -> Option<AgentControl> {
    let (ack, rx) = oneshot::channel();
    if events
        .send(AgentStreamItem::Event(
            Box::new(AgentEvent::ControlRequired { step, outcome }),
            ack,
        ))
        .await
        .is_err()
    {
        return None;
    }
    rx.await.ok()
}

async fn emit_step_completed(
    events: &mpsc::Sender<AgentStreamItem>,
    step: AgentStep,
    outcome: AgentOutcome,
) -> bool {
    emit(events, AgentEvent::StepCompleted { step, outcome }).await
}

async fn emit_step_completed_and_continue(
    events: &mpsc::Sender<AgentStreamItem>,
    run_id: AgentRunId,
    step: AgentStep,
    outcome: AgentOutcome,
) -> Result<bool> {
    let Some(control) = emit_control_required(events, step.clone(), outcome.clone()).await else {
        return Ok(false);
    };
    complete_step_after_control(events, run_id, step, outcome, control).await
}

async fn complete_step_after_control(
    events: &mpsc::Sender<AgentStreamItem>,
    run_id: AgentRunId,
    step: AgentStep,
    outcome: AgentOutcome,
    control: AgentControl,
) -> Result<bool> {
    match control {
        AgentControl::Continue
        | AgentControl::Pause
        | AgentControl::AbortTurn
        | AgentControl::AbortRun => {
            let step_id = step.id;
            if !emit_step_completed(events, step, outcome).await {
                return Ok(false);
            }
            continue_after_control(events, run_id, step_id, control).await
        }
        other => Err(Error::client(format!(
            "agent control {:?} is not valid for this step",
            other.kind()
        ))),
    }
}

async fn continue_after_control(
    events: &mpsc::Sender<AgentStreamItem>,
    run_id: AgentRunId,
    step_id: AgentStepId,
    control: AgentControl,
) -> Result<bool> {
    match control {
        AgentControl::Continue => Ok(true),
        AgentControl::Pause => {
            let _ = emit(
                events,
                AgentEvent::RunPaused {
                    run_id,
                    at: step_id,
                },
            )
            .await;
            Ok(false)
        }
        AgentControl::AbortTurn | AgentControl::AbortRun => {
            let _ = emit(events, AgentEvent::RunAborted { run_id }).await;
            Ok(false)
        }
        other => Err(Error::client(format!(
            "agent control {:?} is not valid for this step",
            other.kind()
        ))),
    }
}

async fn emit_tool_started(
    events: &mpsc::Sender<AgentStreamItem>,
    clock: &mut AgentRunClock,
    tool: ToolCall,
    policy: ToolExecutionPolicy,
) -> Result<bool> {
    let step = clock.next_step(AgentAction::StartTool {
        tool: tool.clone(),
        policy,
    });
    if !emit_step_started(events, &step).await {
        return Ok(false);
    }
    if !emit(
        events,
        AgentEvent::ToolStarted {
            step_id: step.id,
            tool: tool.clone(),
        },
    )
    .await
    {
        return Ok(false);
    }
    let step_id = step.id;
    let outcome = AgentOutcome::ToolStarted { tool };
    let Some(control) = emit_control_required(events, step.clone(), outcome.clone()).await else {
        return Ok(false);
    };
    complete_step_after_control(events, step_id.run_id, step, outcome, control).await
}

async fn emit_tool_finished(
    events: &mpsc::Sender<AgentStreamItem>,
    clock: &mut AgentRunClock,
    tool: ToolCall,
    result: ToolResult,
) -> Result<bool> {
    let step = clock.next_step(AgentAction::ReadTool { tool: tool.clone() });
    if !emit_step_started(events, &step).await {
        return Ok(false);
    }
    let step_id = step.id;
    let outcome = AgentOutcome::ToolFinished { tool, result };
    let Some(control) = emit_control_required(events, step.clone(), outcome.clone()).await else {
        return Ok(false);
    };
    complete_step_after_control(events, step_id.run_id, step, outcome, control).await
}

async fn emit(events: &mpsc::Sender<AgentStreamItem>, event: AgentEvent) -> bool {
    let (ack, rx) = oneshot::channel();
    if events
        .send(AgentStreamItem::Event(Box::new(event), ack))
        .await
        .is_err()
    {
        return false;
    }
    rx.await.is_ok()
}
