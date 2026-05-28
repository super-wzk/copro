use crate::event::{AgentEvent, AgentStream};
use crate::hook::{AgentHook, ToolDecision};
use crate::tools::{ToolExecutionPolicy, ToolRouter};
use copro_api::error::{Error, Result};
use copro_api::message::{
    InputContent, Message, OutputContent, ToolCall, ToolResult, ToolResultStatus,
};
use copro_api::request::{GenerateRequest, GenerateRequestOptions};
use copro_api::stream::{Model, ModelStream, OutputStreamEvent, OutputStreamState};
use futures_util::{StreamExt, future::try_join_all};
use std::sync::Arc;

/// Conversational agent bound to one model with tools, hooks, and conversation state.
pub struct Agent {
    pub model: Arc<dyn Model>,
    pub tools: Arc<dyn ToolRouter>,
    pub hooks: Vec<Arc<dyn AgentHook>>,
    pub max_tool_rounds: usize,
    pub messages: Vec<Message>,
}

impl Agent {
    pub fn new(model: Arc<dyn Model>, tools: Arc<dyn ToolRouter>) -> Self {
        Self {
            model,
            tools,
            hooks: Vec::new(),
            max_tool_rounds: 10,
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
            let mut state = TurnState::ModelRequest;
            let mut model_rounds = 0usize;

            loop {
                state = match state {
                    TurnState::ModelRequest => {
                        if model_rounds >= self.max_tool_rounds {
                            Err(Error::client("max tool rounds exceeded"))?;
                        }
                        model_rounds += 1;

                        let mut request = self.build_request(self.messages.clone()).await?;
                        self.apply_before_request(&mut request).await?;
                        TurnState::ModelStreaming {
                            stream: model.stream(request),
                            output_state: OutputStreamState::new(),
                        }
                    }
                    TurnState::ModelStreaming {
                        mut stream,
                        mut output_state,
                    } => {
                        let event = stream
                            .next()
                            .await
                            .transpose()?
                            .ok_or_else(|| Error::protocol("stream ended before finished event"))?;

                        match event {
                            OutputStreamEvent::Delta {
                                content_index,
                                delta,
                            } => {
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
                                self.apply_before_output_commit(&mut output_content).await?;
                                self.messages.push(normalize_for_history(Message::Assistant(
                                    output_content.clone(),
                                )));
                                self.apply_after_output_commit(&output_content).await?;
                                yield AgentEvent::OutputFinished {
                                    content: output_content.clone(),
                                    reason,
                                    usage,
                                };

                                let tool_calls = output_content
                                    .iter()
                                    .filter_map(|content| match content {
                                        OutputContent::ToolCall(tool) => Some(tool.clone()),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>();

                                if tool_calls.is_empty() {
                                    TurnState::Finished
                                } else {
                                    TurnState::ToolPlanning { tool_calls }
                                }
                            }
                        }
                    }
                    TurnState::ToolPlanning { tool_calls } => {
                        let plan = self.plan_tool_execution(tool_calls).await?;
                        TurnState::ToolExecution { plan }
                    }
                    TurnState::ToolExecution { plan } => {
                        let completed_tools = self.execute_tool_plan(plan).await?;
                        TurnState::ToolResultCommit { completed_tools }
                    }
                    TurnState::ToolResultCommit { completed_tools } => {
                        for (tool, mut result) in completed_tools {
                            self.apply_after_tool_call(&tool, &mut result).await?;
                            self.messages.push(Message::Tool(result.clone()));
                            yield AgentEvent::ToolResult(result);
                        }

                        TurnState::ModelRequest
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
        let mut plan = Vec::new();

        for mut tool in tool_calls {
            match self.apply_before_tool_call(&mut tool).await? {
                ToolDecision::Allow => {
                    let policy = self.tools.execution_policy(&tool).await?;
                    plan.push((tool, policy, None));
                }
                ToolDecision::Reject { reason } => {
                    let result = rejected_tool_result(&tool, reason);
                    plan.push((tool, ToolExecutionPolicy::Serial, Some(result)));
                }
            }
        }

        Ok(plan)
    }

    async fn execute_tool_plan(
        &self,
        plan: Vec<(ToolCall, ToolExecutionPolicy, Option<ToolResult>)>,
    ) -> Result<Vec<(ToolCall, ToolResult)>> {
        let mut completed_tools = Vec::new();
        let mut parallel_batch = Vec::new();

        for (tool, policy, completed_result) in plan {
            if let Some(result) = completed_result {
                completed_tools.extend(self.execute_parallel_batch(&mut parallel_batch).await?);
                completed_tools.push((tool, result));
            } else if policy == ToolExecutionPolicy::Parallel {
                parallel_batch.push(tool);
            } else {
                completed_tools.extend(self.execute_parallel_batch(&mut parallel_batch).await?);
                let result = self.tools.execute(tool.clone()).await?;
                completed_tools.push((tool, result));
            }
        }

        completed_tools.extend(self.execute_parallel_batch(&mut parallel_batch).await?);
        Ok(completed_tools)
    }

    async fn execute_parallel_batch(
        &self,
        batch: &mut Vec<ToolCall>,
    ) -> Result<Vec<(ToolCall, ToolResult)>> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }

        let tools = Arc::clone(&self.tools);
        let calls = std::mem::take(batch);
        try_join_all(calls.into_iter().map(|tool| {
            let tools = Arc::clone(&tools);
            async move {
                let result = tools.execute(tool.clone()).await?;
                Ok((tool, result))
            }
        }))
        .await
    }

    async fn apply_before_request(&self, request: &mut GenerateRequest) -> Result<()> {
        for hook in &self.hooks {
            hook.before_request(request).await?;
        }
        Ok(())
    }

    async fn apply_before_tool_call(&self, tool: &mut ToolCall) -> Result<ToolDecision> {
        for hook in &self.hooks {
            match hook.before_tool_call(tool).await? {
                ToolDecision::Allow => {}
                decision => return Ok(decision),
            }
        }
        Ok(ToolDecision::Allow)
    }

    async fn apply_after_tool_call(&self, tool: &ToolCall, result: &mut ToolResult) -> Result<()> {
        for hook in &self.hooks {
            hook.after_tool_call(tool, result).await?;
        }
        Ok(())
    }

    async fn apply_before_output_commit(&self, content: &mut Vec<OutputContent>) -> Result<()> {
        for hook in &self.hooks {
            hook.before_output_commit(content).await?;
        }
        Ok(())
    }

    async fn apply_after_output_commit(&self, content: &[OutputContent]) -> Result<()> {
        for hook in &self.hooks {
            hook.after_output_commit(content).await?;
        }
        Ok(())
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
    },
    Finished,
}
