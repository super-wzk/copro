use crate::context::{AgentContext, AgentStreamItem};
use crate::event::AgentEvent;
use crate::hook::{AgentHooks, ToolCallDecision};
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
use tokio::sync::{mpsc, oneshot};
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
        let hooks = self.context.hooks.clone();
        let stop_signal = self.context.stop_signal.clone();
        let tool_choice = self.context.tool_choice.clone();
        let hosted_tools = self.context.hosted_tools.clone();
        let options = self.context.options.clone();
        let mut model_stream: Option<ModelStream> = None;

        hooks.before_turn(&mut self.context.messages).await?;
        let mut turn = AgentTurn::new();

        loop {
            if turn.is_finished() {
                break;
            }
            if stop_signal.is_requested() && !turn.needs_tool_result_commit() {
                hooks.after_turn(&self.context.messages).await?;
                turn.finish();
                break;
            }

            match turn.phase() {
                AgentTurnPhase::ModelRequest => {
                    let tool_definitions = tools.definitions().await?;
                    let mut request = turn.build_request(
                        &self.context.messages,
                        tool_definitions,
                        hosted_tools.clone(),
                        tool_choice.clone(),
                        options.clone(),
                    );
                    hooks.before_request(&mut request).await?;
                    model_stream = Some(model.stream(request));
                    turn.open_model_stream()?;
                }
                AgentTurnPhase::ModelStreaming => {
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
                        hooks.after_turn(&self.context.messages).await?;
                        turn.finish();
                        break;
                    };

                    match event {
                        OutputStreamEvent::Delta {
                            content_index,
                            mut delta,
                        } => {
                            hooks.before_output_delta(content_index, &mut delta).await?;
                            turn.apply_output_delta(content_index, delta.clone())?;
                            if !emit(events, AgentEvent::OutputDelta(delta)).await {
                                return Ok(());
                            }
                        }
                        OutputStreamEvent::Finished { reason, usage } => {
                            let mut output = turn.finish_output(reason, usage)?;
                            hooks.before_output_commit(&mut output.content).await?;
                            let committed = turn.commit_output(output)?;
                            self.context
                                .messages
                                .push(normalize_for_history(Message::Assistant(
                                    committed.content.clone(),
                                )));
                            hooks.after_output_commit(&committed.content).await?;
                            if committed.ends_turn {
                                hooks.after_turn(&self.context.messages).await?;
                            }
                            model_stream = None;
                            if !emit(
                                events,
                                AgentEvent::OutputFinished {
                                    content: committed.content,
                                    reason: committed.reason,
                                    usage: committed.usage,
                                },
                            )
                            .await
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
                        let plan = Self::plan_tool_execution(
                            &hooks,
                            Arc::clone(&tools),
                            turn.tool_calls()?,
                        )
                        .await?;
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
                                    if !emit(events, AgentEvent::ToolCallStarted(tool.clone()))
                                        .await
                                    {
                                        return Ok(());
                                    }
                                }
                                completed_tools.extend(
                                    Self::execute_parallel_batch(
                                        Arc::clone(&tools),
                                        stop_signal.token(),
                                        &mut parallel_batch,
                                    )
                                    .await?,
                                );
                                completed_tools.push((tool, result));
                            } else if policy == ToolExecutionPolicy::Parallel {
                                parallel_batch.push(tool);
                            } else {
                                for tool in &parallel_batch {
                                    if !emit(events, AgentEvent::ToolCallStarted(tool.clone()))
                                        .await
                                    {
                                        return Ok(());
                                    }
                                }
                                completed_tools.extend(
                                    Self::execute_parallel_batch(
                                        Arc::clone(&tools),
                                        stop_signal.token(),
                                        &mut parallel_batch,
                                    )
                                    .await?,
                                );
                                if !emit(events, AgentEvent::ToolCallStarted(tool.clone())).await {
                                    return Ok(());
                                }
                                completed_tools.push(
                                    Self::execute_tool_call_with_router(
                                        Arc::clone(&tools),
                                        tool,
                                        stop_signal.token(),
                                    )
                                    .await?,
                                );
                            }
                        }

                        for tool in &parallel_batch {
                            if !emit(events, AgentEvent::ToolCallStarted(tool.clone())).await {
                                return Ok(());
                            }
                        }
                        completed_tools.extend(
                            Self::execute_parallel_batch(
                                Arc::clone(&tools),
                                stop_signal.token(),
                                &mut parallel_batch,
                            )
                            .await?,
                        );
                        turn.set_completed_tools(completed_tools, stop_signal.is_requested())?;
                    }
                }
                AgentTurnPhase::ToolResultCommit => {
                    let pending = turn.take_tool_results_for_commit()?;
                    for (tool, mut result) in pending.completed_tools {
                        hooks.before_tool_result_commit(&tool, &mut result).await?;
                        self.context.messages.push(Message::Tool(result.clone()));
                        hooks.after_tool_result_commit(&tool, &result).await?;
                        if !emit(events, AgentEvent::ToolResult(result)).await {
                            return Ok(());
                        }
                    }

                    let finish_turn = pending.finish_after_commit || stop_signal.is_requested();
                    if finish_turn {
                        hooks.after_turn(&self.context.messages).await?;
                    }
                    turn.finish_tool_result_commit(finish_turn);
                }
                AgentTurnPhase::Finished => break,
            };
        }

        Ok(())
    }

    async fn plan_tool_execution(
        hooks: &AgentHooks,
        tools: Arc<dyn ToolRouter>,
        tool_calls: Vec<ToolCall>,
    ) -> Result<Vec<ToolPlanItem>> {
        let mut tool_calls = tool_calls;
        hooks.before_tool_plan(&mut tool_calls).await?;

        let mut plan = Vec::new();

        for mut tool in tool_calls {
            match hooks.before_tool_call(&mut tool).await? {
                ToolCallDecision::Allow => {
                    let policy = tools.execution_policy(&tool).await?;
                    plan.push(ToolPlanItem::pending(tool, policy));
                }
                ToolCallDecision::Reject { reason } => {
                    let result = rejected_tool_result(&tool, reason);
                    plan.push(ToolPlanItem::completed(tool, result));
                }
            }
        }

        Ok(plan)
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

async fn emit(events: &mpsc::Sender<AgentStreamItem>, event: AgentEvent) -> bool {
    let (ack, rx) = oneshot::channel();
    if events
        .send(AgentStreamItem::Event(event, ack))
        .await
        .is_err()
    {
        return false;
    }
    rx.await.is_ok()
}
