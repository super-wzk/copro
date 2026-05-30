use super::{
    AgentAction, AgentControl, AgentControlSignal, AgentInterruptReason, AgentOutcome, AgentRunId,
    AgentStep, AgentStepId, AgentTurnId,
};
use crate::cancel::RunCancellation;
use crate::context::{AgentContext, AgentStreamItem};
use crate::event::AgentEvent;
use crate::tools::{ToolExecutionPolicy, ToolRouter};
use crate::turn::{
    AgentTurn, AgentTurnPhase, ToolPlanItem, aborted_tool_result, normalize_for_history,
    rejected_tool_result,
};
use copro_api::error::{Error, Result};
use copro_api::message::{
    InputContent, Message, OutputContent, ToolCall, ToolResult, ToolResultStatus,
};
use copro_api::stream::{ModelStream, OutputStreamEvent};
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::any::Any;
use std::collections::VecDeque;
use std::mem;
use std::result::Result as StdResult;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryTerminal {
    AbortTurn,
    AbortRun,
    Preempted { at: AgentStepId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoverableStepFlow {
    Continue,
    Stop,
    Recover(RecoveryTerminal),
}

struct AgentRunClock {
    run_id: AgentRunId,
    turn_id: AgentTurnId,
    next_tick: u64,
    last_step_id: Option<AgentStepId>,
}

impl AgentRunClock {
    fn new(run_id: AgentRunId, turn_id: AgentTurnId) -> Self {
        Self {
            run_id,
            turn_id,
            next_tick: 0,
            last_step_id: None,
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
        self.last_step_id = Some(step.id);
        step
    }

    fn last_step_id(&self) -> Option<AgentStepId> {
        self.last_step_id
    }
}

pub(crate) struct AgentRun<'a> {
    context: &'a mut AgentContext,
    cancellation: RunCancellation,
}

impl<'a> AgentRun<'a> {
    pub(crate) fn new(context: &'a mut AgentContext, cancellation: RunCancellation) -> Self {
        Self {
            context,
            cancellation,
        }
    }

    pub(crate) async fn run_turn(&mut self, events: mpsc::Sender<AgentStreamItem>) {
        if let Err(error) = self.run_turn_inner(&events).await {
            let _ = events.send(AgentStreamItem::Error(error)).await;
        }
    }

    async fn run_turn_inner(&mut self, events: &mpsc::Sender<AgentStreamItem>) -> Result<()> {
        let model = Arc::clone(&self.context.model);
        let tools = Arc::clone(&self.context.tools);
        let cancellation = self.cancellation.clone();
        let tool_choice = self.context.tool_choice.clone();
        let hosted_tools = self.context.hosted_tools.clone();
        let options = self.context.options.clone();
        let mut model_stream: Option<ModelStream> = None;
        let (run_id, turn_id) = self.context.allocate_run_ids();
        let mut clock = AgentRunClock::new(run_id, turn_id);
        let mut terminal_after_recovery = None;

        if !emit(events, AgentEvent::RunStarted { run_id }).await {
            return Ok(());
        }
        if !emit(events, AgentEvent::TurnStarted { run_id, turn_id }).await {
            return Ok(());
        }

        let mut turn = AgentTurn::new();

        'run: loop {
            if turn.is_finished() {
                break;
            }
            if terminal_after_recovery.is_none()
                && cancellation.is_cancelled()
                && !turn.needs_tool_result_commit()
            {
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
                    let mut step_completed = false;
                    match control {
                        AgentControlSignal::Control(AgentControl::Continue) => {}
                        AgentControlSignal::Control(AgentControl::ReplaceRequest(replacement)) => {
                            request = replacement;
                        }
                        other => {
                            if !complete_step_after_control(
                                events,
                                run_id,
                                step.clone(),
                                pending_outcome,
                                other,
                            )
                            .await?
                            {
                                return Ok(());
                            }
                            step_completed = true;
                        }
                    }
                    if !step_completed
                        && !complete_step_after_control(
                            events,
                            run_id,
                            step,
                            AgentOutcome::RequestBuilt(request.clone()),
                            AgentControlSignal::continue_run(),
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
                    let cancel = cancellation.token();
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
                            let mut step_completed = false;
                            let (outcome, delta) = match control {
                                AgentControlSignal::Control(AgentControl::Continue) => (
                                    AgentOutcome::ModelDelta {
                                        content_index,
                                        delta: delta.clone(),
                                    },
                                    Some(delta),
                                ),
                                AgentControlSignal::Control(AgentControl::ReplaceModelDelta(
                                    replacement,
                                )) => (
                                    AgentOutcome::ModelDelta {
                                        content_index,
                                        delta: replacement.clone(),
                                    },
                                    Some(replacement),
                                ),
                                AgentControlSignal::Control(AgentControl::DropModelDelta) => (
                                    AgentOutcome::ModelDeltaDropped {
                                        content_index,
                                        delta,
                                    },
                                    None,
                                ),
                                other => {
                                    if !complete_step_after_control(
                                        events,
                                        run_id,
                                        step.clone(),
                                        pending_outcome.clone(),
                                        other,
                                    )
                                    .await?
                                    {
                                        return Ok(());
                                    }
                                    step_completed = true;
                                    (pending_outcome, Some(delta))
                                }
                            };
                            if !step_completed
                                && !complete_step_after_control(
                                    events,
                                    run_id,
                                    step.clone(),
                                    outcome,
                                    AgentControlSignal::continue_run(),
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
                            let mut step_completed = false;
                            match control {
                                AgentControlSignal::Control(AgentControl::Continue) => {}
                                AgentControlSignal::Control(
                                    AgentControl::ReplaceAssistantOutput(replacement),
                                ) => {
                                    output.content = replacement;
                                }
                                other => {
                                    if !complete_step_after_control(
                                        events,
                                        run_id,
                                        step.clone(),
                                        pending_outcome,
                                        other,
                                    )
                                    .await?
                                    {
                                        return Ok(());
                                    }
                                    step_completed = true;
                                }
                            }
                            if !step_completed
                                && !complete_step_after_control(
                                    events,
                                    run_id,
                                    step,
                                    AgentOutcome::ModelOutputFinished {
                                        content: output.content.clone(),
                                        reason: output.reason,
                                        usage: output.usage.clone(),
                                    },
                                    AgentControlSignal::continue_run(),
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
                                    content: committed.content.clone(),
                                    reason: committed.reason,
                                    usage: committed.usage,
                                },
                            )
                            .await
                            {
                                return Ok(());
                            }
                            if committed
                                .content
                                .iter()
                                .any(|item| matches!(item, OutputContent::ToolCall(_)))
                            {
                                match emit_step_completed_and_continue_recoverable(
                                    events,
                                    run_id,
                                    step.clone(),
                                    outcome.clone(),
                                )
                                .await?
                                {
                                    RecoverableStepFlow::Continue => {}
                                    RecoverableStepFlow::Stop => return Ok(()),
                                    RecoverableStepFlow::Recover(terminal) => {
                                        terminal_after_recovery = Some(terminal);
                                        turn.abort_pending_tools()?;
                                        continue 'run;
                                    }
                                }
                            } else if !emit_step_completed_and_continue(
                                events, run_id, step, outcome,
                            )
                            .await?
                            {
                                return Ok(());
                            }
                        }
                    }
                }
                AgentTurnPhase::ToolPlanning => {
                    if cancellation.is_cancelled() {
                        if !emit_recovering_after_last_step(events, run_id, &clock).await {
                            return Ok(());
                        }
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
                            let mut step_completed = false;
                            match control {
                                AgentControlSignal::Control(AgentControl::Continue) => {
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
                                AgentControlSignal::Control(AgentControl::ReplaceToolCall(
                                    replacement,
                                )) => {
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
                                AgentControlSignal::Control(AgentControl::RejectToolCall {
                                    reason,
                                }) => {
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
                                    final_outcome = pending_outcome.clone();
                                    match complete_recoverable_step_after_control(
                                        events,
                                        run_id,
                                        step.clone(),
                                        pending_outcome,
                                        other,
                                    )
                                    .await?
                                    {
                                        RecoverableStepFlow::Continue => {
                                            step_completed = true;
                                        }
                                        RecoverableStepFlow::Stop => return Ok(()),
                                        RecoverableStepFlow::Recover(terminal) => {
                                            terminal_after_recovery = Some(terminal);
                                            turn.abort_pending_tools()?;
                                            continue 'run;
                                        }
                                    }
                                }
                            }
                            if !step_completed
                                && !complete_step_after_control(
                                    events,
                                    run_id,
                                    step,
                                    final_outcome,
                                    AgentControlSignal::continue_run(),
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
                    if cancellation.is_cancelled() {
                        if !emit_recovering_after_last_step(events, run_id, &clock).await {
                            return Ok(());
                        }
                        turn.abort_pending_tools()?;
                    } else {
                        let mut plan = VecDeque::from(turn.take_tool_plan()?);
                        let mut completed_tools: Vec<(ToolCall, ToolResult)> = Vec::new();
                        let mut parallel_batch: Vec<ToolCall> = Vec::new();

                        while let Some(mut item) = plan.pop_front() {
                            if let Some(result) = item.completed_result.take() {
                                match flush_parallel_batch(
                                    events,
                                    &mut clock,
                                    Arc::clone(&tools),
                                    cancellation.token(),
                                    &mut parallel_batch,
                                    &mut completed_tools,
                                )
                                .await?
                                {
                                    RecoverableStepFlow::Continue => {}
                                    RecoverableStepFlow::Stop => return Ok(()),
                                    RecoverableStepFlow::Recover(terminal) => {
                                        terminal_after_recovery = Some(terminal);
                                        recover_pending_tool_execution(
                                            &mut completed_tools,
                                            &mut parallel_batch,
                                            Some(ToolPlanItem::completed(
                                                item.tool.clone(),
                                                result.clone(),
                                            )),
                                            &mut plan,
                                        );
                                        turn.set_completed_tools(completed_tools, true)?;
                                        continue 'run;
                                    }
                                }
                                completed_tools.push((item.tool, result));
                            } else if item.policy == ToolExecutionPolicy::Parallel {
                                parallel_batch.push(item.tool);
                            } else {
                                match flush_parallel_batch(
                                    events,
                                    &mut clock,
                                    Arc::clone(&tools),
                                    cancellation.token(),
                                    &mut parallel_batch,
                                    &mut completed_tools,
                                )
                                .await?
                                {
                                    RecoverableStepFlow::Continue => {}
                                    RecoverableStepFlow::Stop => return Ok(()),
                                    RecoverableStepFlow::Recover(terminal) => {
                                        terminal_after_recovery = Some(terminal);
                                        recover_pending_tool_execution(
                                            &mut completed_tools,
                                            &mut parallel_batch,
                                            Some(item),
                                            &mut plan,
                                        );
                                        turn.set_completed_tools(completed_tools, true)?;
                                        continue 'run;
                                    }
                                }

                                let tool = item.tool;
                                let policy = item.policy;
                                match emit_tool_started(events, &mut clock, tool.clone(), policy)
                                    .await?
                                {
                                    RecoverableStepFlow::Continue => {}
                                    RecoverableStepFlow::Stop => return Ok(()),
                                    RecoverableStepFlow::Recover(terminal) => {
                                        terminal_after_recovery = Some(terminal);
                                        recover_pending_tool_execution(
                                            &mut completed_tools,
                                            &mut parallel_batch,
                                            Some(ToolPlanItem::pending(tool, policy)),
                                            &mut plan,
                                        );
                                        turn.set_completed_tools(completed_tools, true)?;
                                        continue 'run;
                                    }
                                }
                                let completed = Self::execute_tool_call_with_router(
                                    Arc::clone(&tools),
                                    tool,
                                    cancellation.token(),
                                );
                                let completed = completed.await?;
                                match emit_tool_finished(
                                    events,
                                    &mut clock,
                                    completed.0.clone(),
                                    completed.1.clone(),
                                )
                                .await?
                                {
                                    RecoverableStepFlow::Continue => {
                                        completed_tools.push(completed);
                                    }
                                    RecoverableStepFlow::Stop => return Ok(()),
                                    RecoverableStepFlow::Recover(terminal) => {
                                        completed_tools.push(completed);
                                        terminal_after_recovery = Some(terminal);
                                        recover_pending_tool_execution(
                                            &mut completed_tools,
                                            &mut parallel_batch,
                                            None,
                                            &mut plan,
                                        );
                                        turn.set_completed_tools(completed_tools, true)?;
                                        continue 'run;
                                    }
                                }
                            }
                        }

                        match flush_parallel_batch(
                            events,
                            &mut clock,
                            Arc::clone(&tools),
                            cancellation.token(),
                            &mut parallel_batch,
                            &mut completed_tools,
                        )
                        .await?
                        {
                            RecoverableStepFlow::Continue => {}
                            RecoverableStepFlow::Stop => return Ok(()),
                            RecoverableStepFlow::Recover(terminal) => {
                                terminal_after_recovery = Some(terminal);
                                recover_pending_tool_execution(
                                    &mut completed_tools,
                                    &mut parallel_batch,
                                    None,
                                    &mut plan,
                                );
                                turn.set_completed_tools(completed_tools, true)?;
                                continue 'run;
                            }
                        }
                        if cancellation.is_cancelled()
                            && !emit_recovering_after_last_step(events, run_id, &clock).await
                        {
                            return Ok(());
                        }
                        turn.set_completed_tools(completed_tools, cancellation.is_cancelled())?;
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
                        let mut step_completed = false;
                        match control {
                            AgentControlSignal::Control(AgentControl::Continue) => {}
                            AgentControlSignal::Control(AgentControl::ReplaceToolResult(
                                replacement,
                            )) => {
                                result = replacement;
                            }
                            AgentControlSignal::Control(
                                AgentControl::ReplaceToolResultContent(replacement),
                            ) => {
                                result = ToolResult {
                                    call_id: tool.id.clone(),
                                    name: tool.name.clone(),
                                    status: replacement.status,
                                    content: replacement.content,
                                };
                            }
                            other => {
                                if !complete_step_after_control(
                                    events,
                                    run_id,
                                    step.clone(),
                                    pending_outcome,
                                    other,
                                )
                                .await?
                                {
                                    return Ok(());
                                }
                                step_completed = true;
                            }
                        }
                        if !step_completed
                            && !complete_step_after_control(
                                events,
                                run_id,
                                step,
                                AgentOutcome::ToolResultCommitted {
                                    message_index,
                                    tool: tool.clone(),
                                    result: result.clone(),
                                },
                                AgentControlSignal::continue_run(),
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

                    let finish_turn = pending.finish_after_commit || cancellation.is_cancelled();
                    turn.finish_tool_result_commit(finish_turn);
                }
                AgentTurnPhase::Finished => break,
            };
        }

        if let Some(terminal) = terminal_after_recovery {
            return finish_recovered_run(events, run_id, turn_id, terminal).await;
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

async fn flush_parallel_batch(
    events: &mpsc::Sender<AgentStreamItem>,
    clock: &mut AgentRunClock,
    tools: Arc<dyn ToolRouter>,
    cancel: CancellationToken,
    batch: &mut Vec<ToolCall>,
    completed_tools: &mut Vec<(ToolCall, ToolResult)>,
) -> Result<RecoverableStepFlow> {
    if batch.is_empty() {
        return Ok(RecoverableStepFlow::Continue);
    }

    for tool in batch.iter() {
        match emit_tool_started(events, clock, tool.clone(), ToolExecutionPolicy::Parallel).await? {
            RecoverableStepFlow::Continue => {}
            RecoverableStepFlow::Stop => return Ok(RecoverableStepFlow::Stop),
            RecoverableStepFlow::Recover(terminal) => {
                return Ok(RecoverableStepFlow::Recover(terminal));
            }
        }
    }

    let finished = AgentRun::execute_parallel_batch(tools, cancel, batch).await?;
    let mut terminal = None;
    for (tool, result) in finished {
        if terminal.is_none() {
            match emit_tool_finished(events, clock, tool.clone(), result.clone()).await? {
                RecoverableStepFlow::Continue => {}
                RecoverableStepFlow::Stop => return Ok(RecoverableStepFlow::Stop),
                RecoverableStepFlow::Recover(recovery_terminal) => {
                    terminal = Some(recovery_terminal);
                }
            }
        }
        completed_tools.push((tool, result));
    }

    Ok(terminal.map_or(RecoverableStepFlow::Continue, RecoverableStepFlow::Recover))
}

fn recover_pending_tool_execution(
    completed_tools: &mut Vec<(ToolCall, ToolResult)>,
    parallel_batch: &mut Vec<ToolCall>,
    current: Option<ToolPlanItem>,
    remaining: &mut VecDeque<ToolPlanItem>,
) {
    for tool in mem::take(parallel_batch) {
        let result = aborted_tool_result(&tool);
        completed_tools.push((tool, result));
    }
    if let Some(item) = current {
        recover_tool_plan_item(completed_tools, item);
    }
    while let Some(item) = remaining.pop_front() {
        recover_tool_plan_item(completed_tools, item);
    }
}

fn recover_tool_plan_item(completed_tools: &mut Vec<(ToolCall, ToolResult)>, item: ToolPlanItem) {
    let result = item
        .completed_result
        .unwrap_or_else(|| aborted_tool_result(&item.tool));
    completed_tools.push((item.tool, result));
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

async fn emit_step_completed_and_continue_recoverable(
    events: &mpsc::Sender<AgentStreamItem>,
    run_id: AgentRunId,
    step: AgentStep,
    outcome: AgentOutcome,
) -> Result<RecoverableStepFlow> {
    let Some(control) = emit_control_required(events, step.clone(), outcome.clone()).await else {
        return Ok(RecoverableStepFlow::Stop);
    };
    complete_recoverable_step_after_control(events, run_id, step, outcome, control).await
}

async fn complete_recoverable_step_after_control(
    events: &mpsc::Sender<AgentStreamItem>,
    run_id: AgentRunId,
    step: AgentStep,
    outcome: AgentOutcome,
    signal: AgentControlSignal,
) -> Result<RecoverableStepFlow> {
    if let Some(terminal) = recovery_terminal_for_signal(&signal, step.id) {
        let outcome = outcome_after_recovery_signal(outcome, &signal);
        if !emit_step_completed(events, step.clone(), outcome).await {
            return Ok(RecoverableStepFlow::Stop);
        }
        if !emit_run_recovering(events, run_id, step.id).await {
            return Ok(RecoverableStepFlow::Stop);
        }
        return Ok(RecoverableStepFlow::Recover(terminal));
    }

    if complete_step_after_control(events, run_id, step, outcome, signal).await? {
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
        AgentControlSignal::Control(AgentControl::AbortTurn) => Some(RecoveryTerminal::AbortTurn),
        AgentControlSignal::Control(AgentControl::AbortRun) => Some(RecoveryTerminal::AbortRun),
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

async fn emit_run_recovering(
    events: &mpsc::Sender<AgentStreamItem>,
    run_id: AgentRunId,
    after: AgentStepId,
) -> bool {
    emit(events, AgentEvent::RunRecovering { run_id, after }).await
}

async fn emit_recovering_after_last_step(
    events: &mpsc::Sender<AgentStreamItem>,
    run_id: AgentRunId,
    clock: &AgentRunClock,
) -> bool {
    match clock.last_step_id() {
        Some(after) => emit_run_recovering(events, run_id, after).await,
        None => true,
    }
}

async fn finish_recovered_run(
    events: &mpsc::Sender<AgentStreamItem>,
    run_id: AgentRunId,
    turn_id: AgentTurnId,
    terminal: RecoveryTerminal,
) -> Result<()> {
    match terminal {
        RecoveryTerminal::AbortTurn => {
            if !emit(events, AgentEvent::TurnFinished { run_id, turn_id }).await {
                return Ok(());
            }
            let _ = emit(events, AgentEvent::RunFinished { run_id }).await;
        }
        RecoveryTerminal::AbortRun => {
            let _ = emit(events, AgentEvent::RunAborted { run_id }).await;
        }
        RecoveryTerminal::Preempted { at } => {
            if !emit(events, AgentEvent::RunPreempted { run_id, at }).await {
                return Ok(());
            }
            let _ = emit(events, AgentEvent::RunAborted { run_id }).await;
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
    signal: AgentControlSignal,
) -> Result<bool> {
    match signal {
        AgentControlSignal::Control(
            control @ (AgentControl::Continue | AgentControl::AbortTurn | AgentControl::AbortRun),
        ) => {
            let step_id = step.id;
            if !emit_step_completed(events, step, outcome).await {
                return Ok(false);
            }
            continue_after_control(events, run_id, step_id, control).await
        }
        AgentControlSignal::Control(AgentControl::Pause) => {
            let step_id = step.id;
            if !emit_step_completed(events, step, outcome).await {
                return Ok(false);
            }
            pause_after_control(events, run_id, step_id, None).await
        }
        AgentControlSignal::Pause { resume } => {
            let step_id = step.id;
            if !emit_step_completed(events, step, outcome).await {
                return Ok(false);
            }
            pause_after_control(events, run_id, step_id, Some(resume)).await
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
            if !emit(
                events,
                AgentEvent::RunPreempted {
                    run_id,
                    at: step_id,
                },
            )
            .await
            {
                return Ok(false);
            }
            let _ = emit(events, AgentEvent::RunAborted { run_id }).await;
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
    run_id: AgentRunId,
    step_id: AgentStepId,
    resume: Option<oneshot::Receiver<()>>,
) -> Result<bool> {
    if !emit(
        events,
        AgentEvent::RunPaused {
            run_id,
            at: step_id,
        },
    )
    .await
    {
        return Ok(false);
    }

    let Some(resume) = resume else {
        return Ok(false);
    };
    if resume.await.is_err() {
        return Ok(false);
    }
    if !emit(
        events,
        AgentEvent::RunResumed {
            run_id,
            at: step_id,
        },
    )
    .await
    {
        return Ok(false);
    }
    Ok(true)
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
        AgentControl::AbortTurn => {
            if !emit(
                events,
                AgentEvent::TurnFinished {
                    run_id,
                    turn_id: step_id.turn_id,
                },
            )
            .await
            {
                return Ok(false);
            }
            let _ = emit(events, AgentEvent::RunFinished { run_id }).await;
            Ok(false)
        }
        AgentControl::AbortRun => {
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
) -> Result<RecoverableStepFlow> {
    let step = clock.next_step(AgentAction::StartTool {
        tool: tool.clone(),
        policy,
    });
    if !emit_step_started(events, &step).await {
        return Ok(RecoverableStepFlow::Stop);
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
        return Ok(RecoverableStepFlow::Stop);
    }
    let step_id = step.id;
    let outcome = AgentOutcome::ToolStarted { tool };
    let Some(control) = emit_control_required(events, step.clone(), outcome.clone()).await else {
        return Ok(RecoverableStepFlow::Stop);
    };
    complete_recoverable_step_after_control(events, step_id.run_id, step, outcome, control).await
}

async fn emit_tool_finished(
    events: &mpsc::Sender<AgentStreamItem>,
    clock: &mut AgentRunClock,
    tool: ToolCall,
    result: ToolResult,
) -> Result<RecoverableStepFlow> {
    let step = clock.next_step(AgentAction::ReadTool { tool: tool.clone() });
    if !emit_step_started(events, &step).await {
        return Ok(RecoverableStepFlow::Stop);
    }
    let step_id = step.id;
    let outcome = AgentOutcome::ToolFinished { tool, result };
    let Some(control) = emit_control_required(events, step.clone(), outcome.clone()).await else {
        return Ok(RecoverableStepFlow::Stop);
    };
    complete_recoverable_step_after_control(events, step_id.run_id, step, outcome, control).await
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
