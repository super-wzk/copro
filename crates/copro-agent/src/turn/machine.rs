use crate::tools::ToolExecutionPolicy;
use crate::turn::{AgentAction, AgentOutcome};
use copro_api::error::{Error, Result};
use copro_api::message::{
    InputContent, Message, OutputContent, ToolCall, ToolResult, ToolResultStatus,
};
use copro_api::request::{GenerateRequest, GenerateRequestOptions};
use copro_api::response::{FinishReason, Usage};
use copro_api::stream::{OutputStreamEvent, OutputStreamState};
use copro_api::tool::{HostedToolSpec, ToolChoice, ToolDefinition};
use std::collections::VecDeque;
use std::mem;

pub(crate) struct AgentTurnMachine {
    state: AgentTurnMachineState,
}

impl AgentTurnMachine {
    pub(crate) fn new() -> Self {
        Self {
            state: AgentTurnMachineState::LoadingTools,
        }
    }

    pub(crate) fn next_action(&mut self) -> Result<AgentAction> {
        self.normalize_tool_execution();

        match &self.state {
            AgentTurnMachineState::LoadingTools => Ok(AgentAction::LoadTools),
            AgentTurnMachineState::BuildingRequest { tools } => Ok(AgentAction::BuildRequest {
                tools: tools.clone(),
            }),
            AgentTurnMachineState::OpeningModelStream { request } => {
                Ok(AgentAction::OpenModelStream {
                    request: request.clone(),
                })
            }
            AgentTurnMachineState::ReadingModel { .. } => Ok(AgentAction::ReadModelStream),
            AgentTurnMachineState::CommittingAssistant { output } => {
                Ok(AgentAction::CommitAssistant {
                    content: output.content.clone(),
                    reason: output.reason,
                    usage: output.usage.clone(),
                })
            }
            AgentTurnMachineState::PlanningTools { pending, .. } => {
                let tool = pending
                    .front()
                    .cloned()
                    .ok_or_else(|| unexpected_state("tool planning"))?;
                Ok(AgentAction::PlanTool { tool })
            }
            AgentTurnMachineState::ToolExecution {
                pending,
                active,
                active_policy,
                ..
            } if should_start_next_tool(pending, active, *active_policy) => {
                let item = pending
                    .front()
                    .ok_or_else(|| unexpected_state("tool execution"))?;
                Ok(AgentAction::StartTool {
                    tool: item.tool.clone(),
                    policy: item.policy,
                })
            }
            AgentTurnMachineState::ToolExecution { active, .. } => {
                let tool = active
                    .front()
                    .cloned()
                    .ok_or_else(|| unexpected_state("tool execution"))?;
                Ok(AgentAction::ReadTool { tool })
            }
            AgentTurnMachineState::CommittingToolResults { pending, .. } => {
                let (tool, result) = pending
                    .front()
                    .cloned()
                    .ok_or_else(|| unexpected_state("tool result commit"))?;
                Ok(AgentAction::CommitToolResult { tool, result })
            }
            AgentTurnMachineState::Finishing => Ok(AgentAction::FinishTurn),
            AgentTurnMachineState::Finished => Err(unexpected_state("unfinished turn")),
        }
    }

    pub(crate) fn apply_outcome(
        &mut self,
        action: &AgentAction,
        outcome: AgentOutcome,
    ) -> Result<()> {
        match (action, outcome) {
            (AgentAction::LoadTools, AgentOutcome::ToolsLoaded(tools)) => {
                self.state = AgentTurnMachineState::BuildingRequest { tools };
            }
            (AgentAction::BuildRequest { .. }, AgentOutcome::RequestBuilt(request)) => {
                self.state = AgentTurnMachineState::OpeningModelStream { request };
            }
            (AgentAction::OpenModelStream { .. }, AgentOutcome::ModelStreamOpened) => {
                self.state = AgentTurnMachineState::ReadingModel {
                    output_state: OutputStreamState::new(),
                };
            }
            (
                AgentAction::ReadModelStream,
                AgentOutcome::ModelDelta {
                    content_index,
                    delta,
                },
            ) => {
                let AgentTurnMachineState::ReadingModel { output_state } = &mut self.state else {
                    return Err(unexpected_state("model streaming"));
                };
                output_state.apply(OutputStreamEvent::Delta {
                    content_index,
                    delta,
                })?;
            }
            (AgentAction::ReadModelStream, AgentOutcome::ModelDeltaDropped { .. }) => {}
            (
                AgentAction::ReadModelStream,
                AgentOutcome::ModelOutputFinished {
                    content,
                    reason,
                    usage,
                },
            ) => {
                ensure_reading_model(&self.state)?;
                self.state = AgentTurnMachineState::CommittingAssistant {
                    output: PendingOutput {
                        content,
                        reason,
                        usage,
                    },
                };
            }
            (AgentAction::ReadModelStream, AgentOutcome::ActionInterrupted { .. }) => {
                self.state = AgentTurnMachineState::Finishing;
            }
            (
                AgentAction::CommitAssistant { .. },
                AgentOutcome::AssistantCommitted { content, .. },
            ) => {
                let tool_calls = content
                    .iter()
                    .filter_map(|content| match content {
                        OutputContent::ToolCall(tool) => Some(tool.clone()),
                        _ => None,
                    })
                    .collect::<VecDeque<_>>();
                self.state = if tool_calls.is_empty() {
                    AgentTurnMachineState::Finishing
                } else {
                    AgentTurnMachineState::PlanningTools {
                        pending: tool_calls,
                        plan: Vec::new(),
                    }
                };
            }
            (AgentAction::PlanTool { .. }, AgentOutcome::ToolPlanned { tool, policy }) => {
                self.record_tool_plan(ToolPlanItem::pending(tool, policy))?;
            }
            (AgentAction::PlanTool { .. }, AgentOutcome::ToolRejected { tool, result }) => {
                self.record_tool_plan(ToolPlanItem::completed(tool, result))?;
            }
            (
                AgentAction::StartTool { tool, policy },
                AgentOutcome::ToolStarted { tool: started },
            ) => {
                if tool.id != started.id {
                    return Err(Error::protocol("started tool does not match action"));
                }
                self.record_tool_started(started, *policy)?;
            }
            (
                AgentAction::ReadTool { tool },
                AgentOutcome::ToolFinished {
                    tool: finished,
                    result,
                },
            ) => {
                if tool.id != finished.id {
                    return Err(Error::protocol("finished tool does not match action"));
                }
                self.record_tool_finished(finished, result)?;
            }
            (
                AgentAction::CommitToolResult { tool, .. },
                AgentOutcome::ToolResultCommitted {
                    tool: committed, ..
                },
            ) => {
                if tool.id != committed.id {
                    return Err(Error::protocol(
                        "committed tool result does not match action",
                    ));
                }
                self.record_tool_result_committed()?;
            }
            (AgentAction::FinishTurn, AgentOutcome::TurnFinished) => {
                self.state = AgentTurnMachineState::Finished;
            }
            (action, outcome) => {
                return Err(Error::protocol(format!(
                    "outcome {outcome:?} is not valid for action {action:?}"
                )));
            }
        }

        Ok(())
    }

    pub(crate) fn is_finished(&self) -> bool {
        matches!(self.state, AgentTurnMachineState::Finished)
    }

    pub(crate) fn is_finishing(&self) -> bool {
        matches!(
            self.state,
            AgentTurnMachineState::Finishing | AgentTurnMachineState::Finished
        )
    }

    pub(crate) fn needs_tool_result_commit(&self) -> bool {
        matches!(
            self.state,
            AgentTurnMachineState::PlanningTools { .. }
                | AgentTurnMachineState::ToolExecution { .. }
                | AgentTurnMachineState::CommittingToolResults { .. }
        )
    }

    pub(crate) fn finish(&mut self) {
        self.state = AgentTurnMachineState::Finishing;
    }

    pub(crate) fn build_request(
        &self,
        messages: &[Message],
        tools: Vec<ToolDefinition>,
        hosted_tools: Vec<HostedToolSpec>,
        tool_choice: Option<ToolChoice>,
        options: GenerateRequestOptions,
    ) -> GenerateRequest {
        GenerateRequest {
            messages: messages.to_vec(),
            tools,
            hosted_tools,
            tool_choice,
            options,
        }
    }

    pub(crate) fn model_stream_outcome(&self, event: OutputStreamEvent) -> Result<AgentOutcome> {
        let AgentTurnMachineState::ReadingModel { output_state } = &self.state else {
            return Err(unexpected_state("model streaming"));
        };

        match event {
            OutputStreamEvent::Delta {
                content_index,
                delta,
            } => Ok(AgentOutcome::ModelDelta {
                content_index,
                delta,
            }),
            OutputStreamEvent::Finished { reason, usage } => {
                let mut output_state = output_state.clone();
                let response = output_state
                    .apply(OutputStreamEvent::Finished { reason, usage })?
                    .ok_or_else(|| Error::protocol("stream ended before finished event"))?;

                Ok(AgentOutcome::ModelOutputFinished {
                    content: into_assistant_content(response.message)?,
                    reason: response.reason,
                    usage: response.usage,
                })
            }
        }
    }

    pub(crate) fn recover_pending_tools(&mut self) -> Result<()> {
        self.state = match mem::replace(&mut self.state, AgentTurnMachineState::Finished) {
            AgentTurnMachineState::PlanningTools { pending, plan } => {
                AgentTurnMachineState::CommittingToolResults {
                    pending: recover_tool_planning(pending, plan).into(),
                    finish_after_commit: true,
                }
            }
            AgentTurnMachineState::ToolExecution {
                pending,
                active,
                completed,
                ..
            } => AgentTurnMachineState::CommittingToolResults {
                pending: recover_tool_execution(pending, active, completed).into(),
                finish_after_commit: true,
            },
            AgentTurnMachineState::CommittingToolResults { pending, .. } => {
                AgentTurnMachineState::CommittingToolResults {
                    pending,
                    finish_after_commit: true,
                }
            }
            state => {
                self.state = state;
                return Err(unexpected_state("pending tools"));
            }
        };
        Ok(())
    }

    fn record_tool_plan(&mut self, item: ToolPlanItem) -> Result<()> {
        let AgentTurnMachineState::PlanningTools { pending, plan } = &mut self.state else {
            return Err(unexpected_state("tool planning"));
        };
        let _planned = pending
            .pop_front()
            .ok_or_else(|| unexpected_state("tool planning"))?;
        plan.push(item);
        if pending.is_empty() {
            let pending = VecDeque::from(mem::take(plan));
            self.state = AgentTurnMachineState::ToolExecution {
                pending,
                active: VecDeque::new(),
                active_policy: None,
                completed: Vec::new(),
            };
        }
        Ok(())
    }

    fn record_tool_started(&mut self, tool: ToolCall, policy: ToolExecutionPolicy) -> Result<()> {
        let AgentTurnMachineState::ToolExecution {
            pending,
            active,
            active_policy,
            ..
        } = &mut self.state
        else {
            return Err(unexpected_state("tool execution"));
        };
        let item = pending
            .pop_front()
            .ok_or_else(|| unexpected_state("tool execution"))?;
        if item.tool.id != tool.id || item.policy != policy || item.completed_result.is_some() {
            return Err(Error::protocol("started tool does not match pending plan"));
        }
        if active.is_empty() {
            *active_policy = Some(policy);
        }
        active.push_back(tool);
        Ok(())
    }

    fn record_tool_finished(&mut self, tool: ToolCall, result: ToolResult) -> Result<()> {
        let AgentTurnMachineState::ToolExecution {
            active,
            active_policy,
            completed,
            ..
        } = &mut self.state
        else {
            return Err(unexpected_state("tool execution"));
        };
        let expected = active
            .pop_front()
            .ok_or_else(|| unexpected_state("tool execution"))?;
        if expected.id != tool.id {
            return Err(Error::protocol("finished tool does not match active tool"));
        }
        completed.push((tool, result));
        if active.is_empty() {
            *active_policy = None;
        }
        Ok(())
    }

    fn record_tool_result_committed(&mut self) -> Result<()> {
        let AgentTurnMachineState::CommittingToolResults {
            pending,
            finish_after_commit,
        } = &mut self.state
        else {
            return Err(unexpected_state("tool result commit"));
        };
        pending
            .pop_front()
            .ok_or_else(|| unexpected_state("tool result commit"))?;
        if pending.is_empty() {
            self.state = if *finish_after_commit {
                AgentTurnMachineState::Finished
            } else {
                AgentTurnMachineState::LoadingTools
            };
        }
        Ok(())
    }

    fn normalize_tool_execution(&mut self) {
        let state = mem::replace(&mut self.state, AgentTurnMachineState::Finished);
        let AgentTurnMachineState::ToolExecution {
            mut pending,
            active,
            active_policy,
            mut completed,
        } = state
        else {
            self.state = state;
            return;
        };

        if active.is_empty() {
            while matches!(pending.front(), Some(item) if item.completed_result.is_some()) {
                let item = pending
                    .pop_front()
                    .expect("completed tool plan item must exist");
                let result = item
                    .completed_result
                    .expect("completed tool plan item must have result");
                completed.push((item.tool, result));
            }
        }

        self.state = if pending.is_empty() && active.is_empty() {
            AgentTurnMachineState::CommittingToolResults {
                pending: completed.into(),
                finish_after_commit: false,
            }
        } else {
            AgentTurnMachineState::ToolExecution {
                pending,
                active,
                active_policy,
                completed,
            }
        };
    }
}

pub(crate) struct PendingOutput {
    pub(crate) content: Vec<OutputContent>,
    pub(crate) reason: FinishReason,
    pub(crate) usage: Option<Usage>,
}

pub(crate) struct ToolPlanItem {
    pub(crate) tool: ToolCall,
    pub(crate) policy: ToolExecutionPolicy,
    pub(crate) completed_result: Option<ToolResult>,
}

impl ToolPlanItem {
    pub(crate) fn pending(tool: ToolCall, policy: ToolExecutionPolicy) -> Self {
        Self {
            tool,
            policy,
            completed_result: None,
        }
    }

    pub(crate) fn completed(tool: ToolCall, result: ToolResult) -> Self {
        Self {
            tool,
            policy: ToolExecutionPolicy::Serial,
            completed_result: Some(result),
        }
    }
}

enum AgentTurnMachineState {
    LoadingTools,
    BuildingRequest {
        tools: Vec<ToolDefinition>,
    },
    OpeningModelStream {
        request: GenerateRequest,
    },
    ReadingModel {
        output_state: OutputStreamState,
    },
    CommittingAssistant {
        output: PendingOutput,
    },
    PlanningTools {
        pending: VecDeque<ToolCall>,
        plan: Vec<ToolPlanItem>,
    },
    ToolExecution {
        pending: VecDeque<ToolPlanItem>,
        active: VecDeque<ToolCall>,
        active_policy: Option<ToolExecutionPolicy>,
        completed: Vec<(ToolCall, ToolResult)>,
    },
    CommittingToolResults {
        pending: VecDeque<(ToolCall, ToolResult)>,
        finish_after_commit: bool,
    },
    Finishing,
    Finished,
}

pub(crate) fn rejected_tool_result(tool: &ToolCall, reason: String) -> ToolResult {
    ToolResult {
        call_id: tool.id.clone(),
        name: tool.name.clone(),
        status: ToolResultStatus::Error,
        content: vec![InputContent::Text(reason)],
    }
}

pub(crate) fn aborted_tool_result(tool: &ToolCall) -> ToolResult {
    ToolResult {
        call_id: tool.id.clone(),
        name: tool.name.clone(),
        status: ToolResultStatus::Error,
        content: vec![InputContent::Text("aborted by user".to_string())],
    }
}

pub(crate) fn normalize_for_history(message: Message) -> Message {
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

fn aborted_tool_calls(
    tool_calls: impl IntoIterator<Item = ToolCall>,
) -> Vec<(ToolCall, ToolResult)> {
    tool_calls
        .into_iter()
        .map(|tool| {
            let result = aborted_tool_result(&tool);
            (tool, result)
        })
        .collect()
}

fn abort_tool_plan(plan: impl IntoIterator<Item = ToolPlanItem>) -> Vec<(ToolCall, ToolResult)> {
    plan.into_iter()
        .map(|item| {
            let result = item
                .completed_result
                .unwrap_or_else(|| aborted_tool_result(&item.tool));
            (item.tool, result)
        })
        .collect()
}

fn recover_tool_planning(
    pending: VecDeque<ToolCall>,
    plan: Vec<ToolPlanItem>,
) -> Vec<(ToolCall, ToolResult)> {
    let mut completed = abort_tool_plan(plan);
    completed.extend(aborted_tool_calls(pending));
    completed
}

fn recover_tool_execution(
    pending: VecDeque<ToolPlanItem>,
    active: VecDeque<ToolCall>,
    mut completed: Vec<(ToolCall, ToolResult)>,
) -> Vec<(ToolCall, ToolResult)> {
    completed.extend(aborted_tool_calls(active));
    completed.extend(abort_tool_plan(pending));
    completed
}

fn into_assistant_content(message: Message) -> Result<Vec<OutputContent>> {
    match message {
        Message::Assistant(content) => Ok(content),
        other => Err(Error::protocol(format!(
            "expected assistant message, got {other:?}"
        ))),
    }
}

fn ensure_reading_model(state: &AgentTurnMachineState) -> Result<()> {
    if matches!(state, AgentTurnMachineState::ReadingModel { .. }) {
        Ok(())
    } else {
        Err(unexpected_state("model streaming"))
    }
}

fn should_start_next_tool(
    pending: &VecDeque<ToolPlanItem>,
    active: &VecDeque<ToolCall>,
    active_policy: Option<ToolExecutionPolicy>,
) -> bool {
    let Some(item) = pending.front() else {
        return false;
    };
    if item.completed_result.is_some() {
        return false;
    }
    active.is_empty()
        || (active_policy == Some(ToolExecutionPolicy::Parallel)
            && item.policy == ToolExecutionPolicy::Parallel)
}

fn unexpected_state(expected: &str) -> Error {
    Error::protocol(format!("expected {expected} turn state"))
}
