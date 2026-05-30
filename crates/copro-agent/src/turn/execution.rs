use super::machine::{
    AgentTurnMachine, aborted_tool_result, normalize_for_history, rejected_tool_result,
};
use super::{
    AgentAction, AgentControl, AgentControlSignal, AgentInterruptReason, AgentOutcome, AgentStep,
    AgentStepId, AgentStreamItem, AgentTurnResources,
};
use crate::cancel::TurnCancellation;
use crate::event::AgentEvent;
use crate::tools::ToolRouter;
use copro_api::error::{Error, Result};
use copro_api::message::{
    InputContent, Message, OutputContent, OutputMessage, ToolCall, ToolResult, ToolResultStatus,
};
use copro_api::stream::ModelStream;
use futures_util::StreamExt;
use std::any::Any;
use std::collections::VecDeque;
use std::result::Result as StdResult;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryTerminal {
    FinishTurn,
    AbortTurn,
    Preempted { at: AgentStepId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoverableStepFlow {
    Continue,
    Stop,
    Recover(RecoveryTerminal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentTurnStepFlow {
    Continue,
    Stop,
    Recover(RecoveryTerminal),
}

enum ActionExecution {
    Outcome(AgentOutcome),
    Stop,
}

enum ControlledStepFlow {
    Continue(AgentOutcome),
    Stop,
    Recover(AgentOutcome, RecoveryTerminal),
}

struct AgentTurnRuntime {
    model_stream: Option<ModelStream>,
    running_tools: VecDeque<RunningTool>,
}

struct RunningTool {
    tool: ToolCall,
    handle: AbortOnDropHandle<Result<ToolResult>>,
}

impl AgentTurnRuntime {
    fn new() -> Self {
        Self {
            model_stream: None,
            running_tools: VecDeque::new(),
        }
    }

    fn open_model_stream(&mut self, stream: ModelStream) {
        self.model_stream = Some(stream);
    }

    fn close_model_stream(&mut self) {
        self.model_stream = None;
    }

    fn model_stream(&mut self) -> Result<&mut ModelStream> {
        self.model_stream
            .as_mut()
            .ok_or_else(|| Error::protocol("model stream is not open"))
    }

    fn start_tool(
        &mut self,
        tools: Arc<dyn ToolRouter>,
        tool: ToolCall,
        cancel: CancellationToken,
    ) {
        let tool_cancel = cancel.child_token();
        let tool_for_task = tool.clone();
        let handle = AbortOnDropHandle::new(tokio::spawn(async move {
            tools.execute(tool_for_task, tool_cancel).await
        }));
        self.running_tools.push_back(RunningTool { tool, handle });
    }

    async fn read_tool(
        &mut self,
        expected: &ToolCall,
        cancel: CancellationToken,
    ) -> Result<(ToolCall, ToolResult)> {
        let running = self
            .running_tools
            .pop_front()
            .ok_or_else(|| Error::protocol("tool is not running"))?;
        if running.tool.id != expected.id {
            return Err(Error::protocol("running tool does not match read action"));
        }
        await_running_tool(running, cancel).await
    }

    fn clear_running_tools(&mut self) {
        self.running_tools.clear();
    }
}

struct AgentTurnClock {
    next_tick: u64,
    last_step_id: Option<AgentStepId>,
}

impl AgentTurnClock {
    fn new() -> Self {
        Self {
            next_tick: 0,
            last_step_id: None,
        }
    }

    fn next_step(&mut self, action: AgentAction) -> AgentStep {
        let step = AgentStep {
            id: AgentStepId {
                tick: self.next_tick,
            },
            action,
        };
        self.next_tick += 1;
        self.last_step_id = Some(step.id);
        step
    }

    fn last_step_id(&self) -> Option<AgentStepId> {
        self.last_step_id
    }
}

pub(crate) struct AgentTurn<'a> {
    state: &'a mut AgentTurnResources,
    cancellation: TurnCancellation,
}

impl<'a> AgentTurn<'a> {
    pub(crate) fn new(state: &'a mut AgentTurnResources, cancellation: TurnCancellation) -> Self {
        Self {
            state,
            cancellation,
        }
    }

    pub(crate) async fn execute(&mut self, events: mpsc::Sender<AgentStreamItem>) {
        if let Err(error) = self.execute_inner(&events).await {
            let _ = events.send(AgentStreamItem::Error(error)).await;
        }
    }

    async fn execute_inner(&mut self, events: &mpsc::Sender<AgentStreamItem>) -> Result<()> {
        let mut clock = AgentTurnClock::new();
        let mut runtime = AgentTurnRuntime::new();
        let mut terminal_after_recovery = None;

        if !emit(events, AgentEvent::TurnStarted).await {
            return Ok(());
        }

        let mut turn = AgentTurnMachine::new();

        loop {
            if turn.is_finished() {
                break;
            }

            if terminal_after_recovery.is_none()
                && self.cancellation.is_cancelled()
                && !turn.is_finishing()
            {
                if turn.needs_tool_result_commit() {
                    if !emit_recovering_after_last_step(events, &clock).await {
                        return Ok(());
                    }
                    runtime.clear_running_tools();
                    turn.recover_pending_tools()?;
                    terminal_after_recovery = Some(RecoveryTerminal::FinishTurn);
                } else {
                    turn.finish();
                }
            }

            let action = turn.next_action()?;
            let step = clock.next_step(action);
            match self
                .execute_action_step(events, &mut turn, &mut runtime, step)
                .await?
            {
                AgentTurnStepFlow::Continue => {}
                AgentTurnStepFlow::Stop => return Ok(()),
                AgentTurnStepFlow::Recover(terminal) => {
                    terminal_after_recovery = Some(terminal);
                }
            }
        }

        if let Some(terminal) = terminal_after_recovery {
            return finish_recovered_turn(events, terminal).await;
        }

        Ok(())
    }

    async fn execute_action_step(
        &mut self,
        events: &mpsc::Sender<AgentStreamItem>,
        turn: &mut AgentTurnMachine,
        runtime: &mut AgentTurnRuntime,
        step: AgentStep,
    ) -> Result<AgentTurnStepFlow> {
        if !emit_step_started(events, &step).await {
            return Ok(AgentTurnStepFlow::Stop);
        }

        let pending_outcome = match self.execute_action(events, turn, runtime, &step).await? {
            ActionExecution::Outcome(outcome) => outcome,
            ActionExecution::Stop => return Ok(AgentTurnStepFlow::Stop),
        };

        match self
            .resolve_controlled_step(events, step.clone(), pending_outcome)
            .await?
        {
            ControlledStepFlow::Continue(outcome) => {
                if !self
                    .commit_step_outcome(events, turn, runtime, &step, outcome)
                    .await?
                {
                    return Ok(AgentTurnStepFlow::Stop);
                }
                Ok(AgentTurnStepFlow::Continue)
            }
            ControlledStepFlow::Stop => Ok(AgentTurnStepFlow::Stop),
            ControlledStepFlow::Recover(outcome, terminal) => {
                self.apply_outcome_before_recovery(turn, runtime, &step, outcome)?;
                runtime.clear_running_tools();
                turn.recover_pending_tools()?;
                Ok(AgentTurnStepFlow::Recover(terminal))
            }
        }
    }

    async fn execute_action(
        &mut self,
        events: &mpsc::Sender<AgentStreamItem>,
        turn: &mut AgentTurnMachine,
        runtime: &mut AgentTurnRuntime,
        step: &AgentStep,
    ) -> Result<ActionExecution> {
        match &step.action {
            AgentAction::LoadTools => {
                let tools = self.state.tools.definitions().await?;
                Ok(ActionExecution::Outcome(AgentOutcome::ToolsLoaded(tools)))
            }
            AgentAction::BuildRequest { tools } => {
                let request = turn.build_request(
                    self.state.history.messages(),
                    tools.clone(),
                    self.state.config.hosted_tools().to_vec(),
                    self.state.config.tool_choice().cloned(),
                    self.state.config.options().clone(),
                );
                Ok(ActionExecution::Outcome(AgentOutcome::RequestBuilt(
                    request,
                )))
            }
            AgentAction::OpenModelStream { request } => {
                runtime.open_model_stream(self.state.model.stream(request.clone()));
                Ok(ActionExecution::Outcome(AgentOutcome::ModelStreamOpened))
            }
            AgentAction::ReadModelStream => {
                let cancel = self.cancellation.token();
                let stream = runtime.model_stream()?;
                let event = tokio::select! {
                    _ = cancel.cancelled() => Ok(None),
                    event = stream.next() => match event {
                        Some(Ok(event)) => Ok(Some(event)),
                        Some(Err(error)) => Err(error),
                        None => Err(Error::protocol("stream ended before finished event")),
                    },
                }?;

                let Some(event) = event else {
                    return Ok(ActionExecution::Outcome(AgentOutcome::ActionInterrupted {
                        reason: AgentInterruptReason::Stopped,
                    }));
                };
                Ok(ActionExecution::Outcome(turn.model_stream_outcome(event)?))
            }
            AgentAction::CommitAssistant {
                content,
                reason,
                usage,
            } => {
                let message_index = self.state.history.messages().len();
                self.state
                    .history
                    .push_output(normalize_for_history(OutputMessage::Assistant(
                        content.clone(),
                    )));
                runtime.close_model_stream();
                let outcome = AgentOutcome::AssistantCommitted {
                    message_index,
                    content: content.clone(),
                    reason: *reason,
                    usage: usage.clone(),
                };
                if !emit(
                    events,
                    AgentEvent::AssistantCommitted {
                        step_id: step.id,
                        message_index,
                        content: content.clone(),
                        reason: *reason,
                        usage: usage.clone(),
                    },
                )
                .await
                {
                    return Ok(ActionExecution::Stop);
                }
                turn.apply_outcome(&step.action, outcome.clone())?;
                Ok(ActionExecution::Outcome(outcome))
            }
            AgentAction::PlanTool { tool } => {
                let policy = self.state.tools.execution_policy(tool).await?;
                Ok(ActionExecution::Outcome(AgentOutcome::ToolPlanned {
                    tool: tool.clone(),
                    policy,
                }))
            }
            AgentAction::StartTool { tool, .. } => {
                if !emit(
                    events,
                    AgentEvent::ToolStarted {
                        step_id: step.id,
                        tool: tool.clone(),
                    },
                )
                .await
                {
                    return Ok(ActionExecution::Stop);
                }
                Ok(ActionExecution::Outcome(AgentOutcome::ToolStarted {
                    tool: tool.clone(),
                }))
            }
            AgentAction::ReadTool { tool } => {
                let completed = runtime.read_tool(tool, self.cancellation.token()).await?;
                Ok(ActionExecution::Outcome(AgentOutcome::ToolFinished {
                    tool: completed.0,
                    result: completed.1,
                }))
            }
            AgentAction::CommitToolResult { tool, result } => {
                let message_index = self.state.history.messages().len();
                Ok(ActionExecution::Outcome(
                    AgentOutcome::ToolResultCommitted {
                        message_index,
                        tool: tool.clone(),
                        result: result.clone(),
                    },
                ))
            }
            AgentAction::FinishTurn => Ok(ActionExecution::Outcome(AgentOutcome::TurnFinished)),
        }
    }

    async fn resolve_controlled_step(
        &mut self,
        events: &mpsc::Sender<AgentStreamItem>,
        step: AgentStep,
        pending_outcome: AgentOutcome,
    ) -> Result<ControlledStepFlow> {
        let Some(control) =
            emit_control_required(events, step.clone(), pending_outcome.clone()).await
        else {
            return Ok(ControlledStepFlow::Stop);
        };
        let (outcome, control) = self
            .resolve_controlled_outcome(pending_outcome, control)
            .await?;

        if outcome_allows_recovery(&outcome) {
            match complete_recoverable_step_after_control(events, step, outcome.clone(), control)
                .await?
            {
                RecoverableStepFlow::Continue => Ok(ControlledStepFlow::Continue(outcome)),
                RecoverableStepFlow::Stop => Ok(ControlledStepFlow::Stop),
                RecoverableStepFlow::Recover(terminal) => {
                    Ok(ControlledStepFlow::Recover(outcome, terminal))
                }
            }
        } else if complete_step_after_control(events, step, outcome.clone(), control).await? {
            Ok(ControlledStepFlow::Continue(outcome))
        } else {
            Ok(ControlledStepFlow::Stop)
        }
    }

    async fn resolve_controlled_outcome(
        &mut self,
        pending_outcome: AgentOutcome,
        control: AgentControlSignal,
    ) -> Result<(AgentOutcome, AgentControlSignal)> {
        match (pending_outcome, control) {
            (
                AgentOutcome::RequestBuilt(_),
                AgentControlSignal::Control(AgentControl::ReplaceRequest(request)),
            ) => Ok((
                AgentOutcome::RequestBuilt(request),
                AgentControlSignal::continue_turn(),
            )),
            (
                AgentOutcome::ModelDelta {
                    content_index,
                    delta: _,
                },
                AgentControlSignal::Control(AgentControl::ReplaceModelDelta(delta)),
            ) => Ok((
                AgentOutcome::ModelDelta {
                    content_index,
                    delta,
                },
                AgentControlSignal::continue_turn(),
            )),
            (
                AgentOutcome::ModelDelta {
                    content_index,
                    delta,
                },
                AgentControlSignal::Control(AgentControl::DropModelDelta),
            ) => Ok((
                AgentOutcome::ModelDeltaDropped {
                    content_index,
                    delta,
                },
                AgentControlSignal::continue_turn(),
            )),
            (
                AgentOutcome::ModelOutputFinished { reason, usage, .. },
                AgentControlSignal::Control(AgentControl::ReplaceAssistantOutput(content)),
            ) => Ok((
                AgentOutcome::ModelOutputFinished {
                    content,
                    reason,
                    usage,
                },
                AgentControlSignal::continue_turn(),
            )),
            (
                AgentOutcome::ToolPlanned { tool, .. } | AgentOutcome::ToolRejected { tool, .. },
                AgentControlSignal::Control(AgentControl::ReplaceToolCall(replacement)),
            ) => {
                replace_tool_call_in_history(
                    self.state.history.messages_mut(),
                    &tool,
                    replacement.clone(),
                );
                let policy = self.state.tools.execution_policy(&replacement).await?;
                Ok((
                    AgentOutcome::ToolPlanned {
                        tool: replacement,
                        policy,
                    },
                    AgentControlSignal::continue_turn(),
                ))
            }
            (
                AgentOutcome::ToolPlanned { tool, .. } | AgentOutcome::ToolRejected { tool, .. },
                AgentControlSignal::Control(AgentControl::RejectToolCall { reason }),
            ) => {
                let result = rejected_tool_result(&tool, reason);
                Ok((
                    AgentOutcome::ToolRejected { tool, result },
                    AgentControlSignal::continue_turn(),
                ))
            }
            (
                AgentOutcome::ToolResultCommitted {
                    message_index,
                    tool,
                    ..
                },
                AgentControlSignal::Control(AgentControl::ReplaceToolResult(result)),
            ) => Ok((
                AgentOutcome::ToolResultCommitted {
                    message_index,
                    tool,
                    result,
                },
                AgentControlSignal::continue_turn(),
            )),
            (
                AgentOutcome::ToolResultCommitted {
                    message_index,
                    tool,
                    ..
                },
                AgentControlSignal::Control(AgentControl::ReplaceToolResultContent(replacement)),
            ) => {
                let result = ToolResult {
                    call_id: tool.id.clone(),
                    name: tool.name.clone(),
                    status: replacement.status,
                    content: replacement.content,
                };
                Ok((
                    AgentOutcome::ToolResultCommitted {
                        message_index,
                        tool,
                        result,
                    },
                    AgentControlSignal::continue_turn(),
                ))
            }
            (outcome, control) => Ok((outcome, control)),
        }
    }

    async fn commit_step_outcome(
        &mut self,
        events: &mpsc::Sender<AgentStreamItem>,
        turn: &mut AgentTurnMachine,
        runtime: &mut AgentTurnRuntime,
        step: &AgentStep,
        outcome: AgentOutcome,
    ) -> Result<bool> {
        match (&step.action, outcome) {
            (
                _,
                AgentOutcome::ModelDelta {
                    content_index,
                    delta,
                },
            ) => {
                let outcome = AgentOutcome::ModelDelta {
                    content_index,
                    delta: delta.clone(),
                };
                turn.apply_outcome(&step.action, outcome)?;
                Ok(emit(
                    events,
                    AgentEvent::ModelDelta {
                        step_id: step.id,
                        content_index,
                        delta,
                    },
                )
                .await)
            }
            (_, outcome @ AgentOutcome::ModelDeltaDropped { .. }) => {
                turn.apply_outcome(&step.action, outcome)?;
                Ok(true)
            }
            (_, outcome @ AgentOutcome::ModelOutputFinished { .. }) => {
                turn.apply_outcome(&step.action, outcome)?;
                Ok(true)
            }
            (AgentAction::CommitAssistant { .. }, AgentOutcome::AssistantCommitted { .. }) => {
                Ok(true)
            }
            (AgentAction::StartTool { policy, .. }, AgentOutcome::ToolStarted { tool }) => {
                runtime.start_tool(
                    Arc::clone(&self.state.tools),
                    tool.clone(),
                    self.cancellation.token(),
                );
                turn.apply_outcome(
                    &AgentAction::StartTool {
                        tool: tool.clone(),
                        policy: *policy,
                    },
                    AgentOutcome::ToolStarted { tool },
                )?;
                Ok(true)
            }
            (
                AgentAction::CommitToolResult { .. },
                AgentOutcome::ToolResultCommitted {
                    message_index,
                    tool,
                    result,
                },
            ) => {
                self.state
                    .history
                    .push_output(OutputMessage::Tool(result.clone()));
                if !emit(
                    events,
                    AgentEvent::ToolResultCommitted {
                        step_id: step.id,
                        message_index,
                        tool: tool.clone(),
                        result: result.clone(),
                    },
                )
                .await
                {
                    return Ok(false);
                }
                turn.apply_outcome(
                    &step.action,
                    AgentOutcome::ToolResultCommitted {
                        message_index,
                        tool,
                        result,
                    },
                )?;
                Ok(true)
            }
            (AgentAction::FinishTurn, outcome @ AgentOutcome::TurnFinished) => {
                if !emit(events, AgentEvent::TurnFinished).await {
                    return Ok(false);
                }
                turn.apply_outcome(&step.action, outcome)?;
                Ok(true)
            }
            (_, outcome) => {
                turn.apply_outcome(&step.action, outcome)?;
                Ok(true)
            }
        }
    }

    fn apply_outcome_before_recovery(
        &mut self,
        turn: &mut AgentTurnMachine,
        _runtime: &mut AgentTurnRuntime,
        step: &AgentStep,
        outcome: AgentOutcome,
    ) -> Result<()> {
        if matches!(step.action, AgentAction::ReadTool { .. }) {
            turn.apply_outcome(&step.action, outcome)?;
        }
        Ok(())
    }
}

async fn await_running_tool(
    mut running: RunningTool,
    cancel: CancellationToken,
) -> Result<(ToolCall, ToolResult)> {
    let tool_for_abort = running.tool.clone();
    tokio::select! {
        biased;
        result = &mut running.handle => join_tool_task_result(running.tool, result),
        _ = cancel.cancelled() => {
            tokio::select! {
                biased;
                result = &mut running.handle => join_tool_task_result(running.tool, result),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    running.handle.abort();
                    Ok((tool_for_abort.clone(), aborted_tool_result(&tool_for_abort)))
                }
            }
        }
    }
}

fn outcome_allows_recovery(outcome: &AgentOutcome) -> bool {
    match outcome {
        AgentOutcome::AssistantCommitted { content, .. } => content
            .iter()
            .any(|item| matches!(item, OutputContent::ToolCall(_))),
        AgentOutcome::ToolPlanned { .. }
        | AgentOutcome::ToolRejected { .. }
        | AgentOutcome::ToolStarted { .. }
        | AgentOutcome::ToolFinished { .. } => true,
        _ => false,
    }
}

fn replace_tool_call_in_history(
    messages: &mut [Message],
    original: &ToolCall,
    replacement: ToolCall,
) {
    for message in messages.iter_mut().rev() {
        let Message::Output(OutputMessage::Assistant(content)) = message else {
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

async fn complete_recoverable_step_after_control(
    events: &mpsc::Sender<AgentStreamItem>,
    step: AgentStep,
    outcome: AgentOutcome,
    signal: AgentControlSignal,
) -> Result<RecoverableStepFlow> {
    if let Some(terminal) = recovery_terminal_for_signal(&signal, step.id) {
        let outcome = outcome_after_recovery_signal(outcome, &signal);
        if !emit_step_completed(events, step.clone(), outcome).await {
            return Ok(RecoverableStepFlow::Stop);
        }
        if !emit_turn_recovering(events, step.id).await {
            return Ok(RecoverableStepFlow::Stop);
        }
        return Ok(RecoverableStepFlow::Recover(terminal));
    }

    if complete_step_after_control(events, step, outcome, signal).await? {
        Ok(RecoverableStepFlow::Continue)
    } else {
        Ok(RecoverableStepFlow::Stop)
    }
}

fn recovery_terminal_for_signal(
    signal: &AgentControlSignal,
    step_id: AgentStepId,
) -> Option<RecoveryTerminal> {
    match signal {
        AgentControlSignal::Control(AgentControl::FinishTurn) => Some(RecoveryTerminal::FinishTurn),
        AgentControlSignal::Control(AgentControl::AbortTurn) => Some(RecoveryTerminal::AbortTurn),
        AgentControlSignal::Preempt => Some(RecoveryTerminal::Preempted { at: step_id }),
        _ => None,
    }
}

fn outcome_after_recovery_signal(
    outcome: AgentOutcome,
    signal: &AgentControlSignal,
) -> AgentOutcome {
    match signal {
        AgentControlSignal::Preempt => match outcome {
            AgentOutcome::ActionInterrupted { .. } => AgentOutcome::ActionInterrupted {
                reason: AgentInterruptReason::Preempted,
            },
            outcome => outcome,
        },
        _ => outcome,
    }
}

async fn emit_turn_recovering(events: &mpsc::Sender<AgentStreamItem>, after: AgentStepId) -> bool {
    emit(events, AgentEvent::TurnRecovering { after }).await
}

async fn emit_recovering_after_last_step(
    events: &mpsc::Sender<AgentStreamItem>,
    clock: &AgentTurnClock,
) -> bool {
    match clock.last_step_id() {
        Some(after) => emit_turn_recovering(events, after).await,
        None => true,
    }
}

async fn finish_recovered_turn(
    events: &mpsc::Sender<AgentStreamItem>,
    terminal: RecoveryTerminal,
) -> Result<()> {
    match terminal {
        RecoveryTerminal::FinishTurn => {
            let _ = emit(events, AgentEvent::TurnFinished).await;
        }
        RecoveryTerminal::AbortTurn => {
            let _ = emit(events, AgentEvent::TurnAborted).await;
        }
        RecoveryTerminal::Preempted { at } => {
            if !emit(events, AgentEvent::TurnPreempted { at }).await {
                return Ok(());
            }
            let _ = emit(events, AgentEvent::TurnAborted).await;
        }
    }
    Ok(())
}

async fn emit_step_started(events: &mpsc::Sender<AgentStreamItem>, step: &AgentStep) -> bool {
    if !emit(events, AgentEvent::StepReady { step: step.clone() }).await {
        return false;
    }
    emit(events, AgentEvent::StepStarted { step: step.clone() }).await
}

async fn emit_control_required(
    events: &mpsc::Sender<AgentStreamItem>,
    step: AgentStep,
    outcome: AgentOutcome,
) -> Option<AgentControlSignal> {
    let (reply, rx) = oneshot::channel();
    if events
        .send(AgentStreamItem::ControlRequired {
            step: Box::new(step),
            outcome: Box::new(outcome),
            reply,
        })
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

async fn complete_step_after_control(
    events: &mpsc::Sender<AgentStreamItem>,
    step: AgentStep,
    outcome: AgentOutcome,
    signal: AgentControlSignal,
) -> Result<bool> {
    match signal {
        AgentControlSignal::Control(
            control @ (AgentControl::Continue | AgentControl::FinishTurn | AgentControl::AbortTurn),
        ) => {
            let step_id = step.id;
            if !emit_step_completed(events, step, outcome).await {
                return Ok(false);
            }
            continue_after_control(events, step_id, control).await
        }
        AgentControlSignal::Control(AgentControl::Pause) => {
            let step_id = step.id;
            if !emit_step_completed(events, step, outcome).await {
                return Ok(false);
            }
            pause_after_control(events, step_id, None).await
        }
        AgentControlSignal::Pause { resume } => {
            let step_id = step.id;
            if !emit_step_completed(events, step, outcome).await {
                return Ok(false);
            }
            pause_after_control(events, step_id, Some(resume)).await
        }
        AgentControlSignal::Preempt => {
            let step_id = step.id;
            let outcome = match outcome {
                AgentOutcome::ActionInterrupted { .. } => AgentOutcome::ActionInterrupted {
                    reason: AgentInterruptReason::Preempted,
                },
                outcome => outcome,
            };
            if !emit_step_completed(events, step, outcome).await {
                return Ok(false);
            }
            if !emit(events, AgentEvent::TurnPreempted { at: step_id }).await {
                return Ok(false);
            }
            let _ = emit(events, AgentEvent::TurnAborted).await;
            Ok(false)
        }
        other => Err(Error::client(format!(
            "agent control {:?} is not valid for this step",
            other.kind()
        ))),
    }
}

async fn pause_after_control(
    events: &mpsc::Sender<AgentStreamItem>,
    step_id: AgentStepId,
    resume: Option<oneshot::Receiver<()>>,
) -> Result<bool> {
    if !emit(events, AgentEvent::TurnPaused { at: step_id }).await {
        return Ok(false);
    }

    let Some(resume) = resume else {
        return Ok(false);
    };
    if resume.await.is_err() {
        return Ok(false);
    }
    if !emit(events, AgentEvent::TurnResumed { at: step_id }).await {
        return Ok(false);
    }
    Ok(true)
}

async fn continue_after_control(
    events: &mpsc::Sender<AgentStreamItem>,
    step_id: AgentStepId,
    control: AgentControl,
) -> Result<bool> {
    match control {
        AgentControl::Continue => Ok(true),
        AgentControl::Pause => {
            let _ = emit(events, AgentEvent::TurnPaused { at: step_id }).await;
            Ok(false)
        }
        AgentControl::FinishTurn => {
            let _ = emit(events, AgentEvent::TurnFinished).await;
            Ok(false)
        }
        AgentControl::AbortTurn => {
            let _ = emit(events, AgentEvent::TurnAborted).await;
            Ok(false)
        }
        other => Err(Error::client(format!(
            "agent control {:?} is not valid for this step",
            other.kind()
        ))),
    }
}

async fn emit(events: &mpsc::Sender<AgentStreamItem>, event: AgentEvent) -> bool {
    events
        .send(AgentStreamItem::Event(Box::new(event)))
        .await
        .is_ok()
}
