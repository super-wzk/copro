use crate::event::{AgentEvent, AgentStream};
use crate::hook::{AgentHooks, ToolCallDecision};
use crate::runtime::StopSignal;
use crate::tools::{ToolExecutionPolicy, ToolRouter};
use copro_api::error::{Error, Result};
use copro_api::message::{
    InputContent, Message, OutputContent, ToolCall, ToolResult, ToolResultStatus,
};
use copro_api::request::{GenerateRequest, GenerateRequestOptions};
use copro_api::stream::{Model, ModelStream, OutputStreamEvent, OutputStreamState};
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::any::Any;
use std::mem;
use std::result::Result as StdResult;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

/// Conversational agent bound to one model with tools, hooks, and conversation state.
pub struct Agent {
    pub model: Arc<dyn Model>,
    pub tools: Arc<dyn ToolRouter>,
    pub hooks: AgentHooks,
    pub stop_signal: StopSignal,
    pub messages: Vec<Message>,
}

impl Agent {
    pub fn new(model: Arc<dyn Model>, tools: Arc<dyn ToolRouter>) -> Self {
        Self {
            model,
            tools,
            hooks: AgentHooks::new(),
            stop_signal: StopSignal::new(),
            messages: Vec::new(),
        }
    }

    /// Run one streaming turn using this agent's bound model and conversation state.
    ///
    /// Model content is streamed as [`AgentEvent::OutputDelta`] events.
    /// Completed model outputs and tool results are yielded when they are
    /// committed to state. Stream completion marks the end of the turn.
    pub fn run_stream(&mut self) -> AgentStream<'_> {
        Box::pin(async_stream::try_stream! {
            let model = Arc::clone(&self.model);
            self.hooks.before_turn(&mut self.messages).await?;
            let mut state = TurnState::ModelRequest;

            loop {
                if state.is_finished() {
                    break;
                }
                if self.stop_signal.is_requested() && !state.needs_tool_result_commit() {
                    self.hooks.after_turn(&self.messages).await?;
                    break;
                }

                state = match state {
                    TurnState::ModelRequest => {
                        let mut request = self.build_request(self.messages.clone()).await?;
                        self.hooks.before_request(&mut request).await?;
                        TurnState::ModelStreaming {
                            stream: model.stream(request),
                            output_state: OutputStreamState::new(),
                        }
                    }
                    TurnState::ModelStreaming {
                        mut stream,
                        mut output_state,
                    } => {
                        let cancel = self.stop_signal.token();
                        let event = tokio::select! {
                            _ = cancel.cancelled() => {
                                self.hooks.after_turn(&self.messages).await.map(|()| None)
                            }
                            event = stream.next() => match event {
                                Some(Ok(event)) => Ok(Some(event)),
                                Some(Err(error)) => Err(error),
                                None => Err(Error::protocol("stream ended before finished event")),
                            },
                        }?;

                        match event {
                            Some(event) => match event {
                                OutputStreamEvent::Delta {
                                    content_index,
                                    mut delta,
                                } => {
                                    self.hooks.before_output_delta(content_index, &mut delta).await?;
                                    yield AgentEvent::OutputDelta(delta.clone());
                                    output_state.apply(OutputStreamEvent::Delta {
                                        content_index,
                                        delta,
                                    })?;
                                    TurnState::ModelStreaming {
                                        stream,
                                        output_state,
                                    }
                                }
                                OutputStreamEvent::Finished { reason, usage } => {
                                    let response = output_state
                                        .apply(OutputStreamEvent::Finished { reason, usage })?
                                        .ok_or_else(|| {
                                            Error::protocol("stream ended before finished event")
                                        })?;

                                    let usage = response.usage.clone();
                                    let mut output_content = into_assistant_content(response.message)?;
                                    self.hooks.before_output_commit(&mut output_content).await?;
                                    self.messages.push(normalize_for_history(Message::Assistant(
                                        output_content.clone(),
                                    )));
                                    self.hooks.after_output_commit(&output_content).await?;
                                    let tool_calls = output_content
                                        .iter()
                                        .filter_map(|content| match content {
                                            OutputContent::ToolCall(tool) => Some(tool.clone()),
                                            _ => None,
                                        })
                                        .collect::<Vec<_>>();
                                    let next_state = if tool_calls.is_empty() {
                                        self.hooks.after_turn(&self.messages).await?;
                                        TurnState::Finished
                                    } else {
                                        TurnState::ToolPlanning { tool_calls }
                                    };

                                    yield AgentEvent::OutputFinished {
                                        content: output_content.clone(),
                                        reason,
                                        usage,
                                    };

                                    next_state
                                }
                            },
                            None => TurnState::Finished,
                        }
                    }
                    TurnState::ToolPlanning { tool_calls } => {
                        if self.stop_signal.is_requested() {
                            TurnState::ToolResultCommit {
                                completed_tools: aborted_tool_calls(tool_calls),
                                finish_after_commit: true,
                            }
                        } else {
                            let plan = self.plan_tool_execution(tool_calls).await?;
                            TurnState::ToolExecution { plan }
                        }
                    }
                    TurnState::ToolExecution { plan } => {
                        if self.stop_signal.is_requested() {
                            TurnState::ToolResultCommit {
                                completed_tools: abort_tool_plan(plan),
                                finish_after_commit: true,
                            }
                        } else {
                            let mut completed_tools: Vec<(ToolCall, ToolResult)> = Vec::new();
                            let mut parallel_batch: Vec<ToolCall> = Vec::new();

                            for (tool, policy, completed_result) in plan {
                                if let Some(result) = completed_result {
                                    for tool in &parallel_batch {
                                        yield AgentEvent::ToolCallStarted(tool.clone());
                                    }
                                    completed_tools.extend(
                                        self.execute_parallel_batch(&mut parallel_batch).await?,
                                    );
                                    completed_tools.push((tool, result));
                                } else if policy == ToolExecutionPolicy::Parallel {
                                    parallel_batch.push(tool);
                                } else {
                                    for tool in &parallel_batch {
                                        yield AgentEvent::ToolCallStarted(tool.clone());
                                    }
                                    completed_tools.extend(
                                        self.execute_parallel_batch(&mut parallel_batch).await?,
                                    );
                                    yield AgentEvent::ToolCallStarted(tool.clone());
                                    completed_tools.push(
                                        self.execute_tool_call(tool, self.stop_signal.token())
                                            .await?,
                                    );
                                }
                            }

                            for tool in &parallel_batch {
                                yield AgentEvent::ToolCallStarted(tool.clone());
                            }
                            completed_tools
                                .extend(self.execute_parallel_batch(&mut parallel_batch).await?);
                            TurnState::ToolResultCommit {
                                completed_tools,
                                finish_after_commit: self.stop_signal.is_requested(),
                            }
                        }
                    }
                    TurnState::ToolResultCommit {
                        completed_tools,
                        finish_after_commit,
                    } => {
                        for (tool, mut result) in completed_tools {
                            self.hooks.before_tool_result_commit(&tool, &mut result).await?;
                            self.messages.push(Message::Tool(result.clone()));
                            self.hooks.after_tool_result_commit(&tool, &result).await?;
                            yield AgentEvent::ToolResult(result);
                        }

                        if finish_after_commit || self.stop_signal.is_requested() {
                            self.hooks.after_turn(&self.messages).await?;
                            TurnState::Finished
                        } else {
                            TurnState::ModelRequest
                        }
                    }
                    TurnState::Finished => break,
                };
            }
        })
    }

    async fn build_request(&self, messages: Vec<Message>) -> Result<GenerateRequest> {
        Ok(GenerateRequest {
            messages,
            tools: self.tools.definitions().await?,
            hosted_tools: Vec::new(),
            tool_choice: None,
            options: GenerateRequestOptions::default(),
        })
    }

    async fn plan_tool_execution(
        &self,
        tool_calls: Vec<ToolCall>,
    ) -> Result<Vec<(ToolCall, ToolExecutionPolicy, Option<ToolResult>)>> {
        let mut tool_calls = tool_calls;
        self.hooks.before_tool_plan(&mut tool_calls).await?;

        let mut plan = Vec::new();

        for mut tool in tool_calls {
            match self.hooks.before_tool_call(&mut tool).await? {
                ToolCallDecision::Allow => {
                    let policy = self.tools.execution_policy(&tool).await?;
                    plan.push((tool, policy, None));
                }
                ToolCallDecision::Reject { reason } => {
                    let result = rejected_tool_result(&tool, reason);
                    plan.push((tool, ToolExecutionPolicy::Serial, Some(result)));
                }
            }
        }

        Ok(plan)
    }

    async fn execute_tool_call(
        &self,
        tool: ToolCall,
        cancel: CancellationToken,
    ) -> Result<(ToolCall, ToolResult)> {
        Self::execute_tool_call_with_router(Arc::clone(&self.tools), tool, cancel).await
    }

    async fn execute_parallel_batch(
        &self,
        batch: &mut Vec<ToolCall>,
    ) -> Result<Vec<(ToolCall, ToolResult)>> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }

        let tools = Arc::clone(&self.tools);
        let cancel = self.stop_signal.token();
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

fn rejected_tool_result(tool: &ToolCall, reason: String) -> ToolResult {
    ToolResult {
        call_id: tool.id.clone(),
        name: tool.name.clone(),
        status: ToolResultStatus::Error,
        content: vec![InputContent::Text(reason)],
    }
}

fn aborted_tool_calls(tool_calls: Vec<ToolCall>) -> Vec<(ToolCall, ToolResult)> {
    tool_calls
        .into_iter()
        .map(|tool| {
            let result = aborted_tool_result(&tool);
            (tool, result)
        })
        .collect()
}

fn abort_tool_plan(
    plan: Vec<(ToolCall, ToolExecutionPolicy, Option<ToolResult>)>,
) -> Vec<(ToolCall, ToolResult)> {
    plan.into_iter()
        .map(|(tool, _policy, result)| {
            let result = result.unwrap_or_else(|| aborted_tool_result(&tool));
            (tool, result)
        })
        .collect()
}

fn aborted_tool_result(tool: &ToolCall) -> ToolResult {
    ToolResult {
        call_id: tool.id.clone(),
        name: tool.name.clone(),
        status: ToolResultStatus::Error,
        content: vec![InputContent::Text("aborted by user".to_string())],
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

fn into_assistant_content(message: Message) -> Result<Vec<OutputContent>> {
    match message {
        Message::Assistant(content) => Ok(content),
        other => Err(Error::protocol(format!(
            "expected assistant message, got {other:?}"
        ))),
    }
}

fn normalize_for_history(message: Message) -> Message {
    match message {
        Message::Assistant(content) => Message::Assistant(
            content
                .into_iter()
                .filter(|c| !matches!(c, OutputContent::Thinking(_) | OutputContent::Image(_)))
                .collect(),
        ),
        other => other,
    }
}

impl<'a> TurnState<'a> {
    fn is_finished(&self) -> bool {
        matches!(self, Self::Finished)
    }

    fn needs_tool_result_commit(&self) -> bool {
        matches!(
            self,
            Self::ToolPlanning { .. } | Self::ToolExecution { .. } | Self::ToolResultCommit { .. }
        )
    }
}

enum TurnState<'a> {
    ModelRequest,
    ModelStreaming {
        stream: ModelStream<'a>,
        output_state: OutputStreamState,
    },
    ToolPlanning {
        tool_calls: Vec<ToolCall>,
    },
    ToolExecution {
        plan: Vec<(ToolCall, ToolExecutionPolicy, Option<ToolResult>)>,
    },
    ToolResultCommit {
        completed_tools: Vec<(ToolCall, ToolResult)>,
        finish_after_commit: bool,
    },
    Finished,
}
