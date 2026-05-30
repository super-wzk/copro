use crate::tools::ToolExecutionPolicy;
use copro_api::error::{Error, Result};
use copro_api::message::{
    InputContent, Message, OutputContent, ToolCall, ToolResult, ToolResultStatus,
};
use copro_api::request::{GenerateRequest, GenerateRequestOptions};
use copro_api::response::{FinishReason, Usage};
use copro_api::stream::{OutputContentDelta, OutputStreamEvent, OutputStreamState};
use copro_api::tool::{HostedToolSpec, ToolChoice, ToolDefinition};
use std::mem;

pub(crate) struct AgentTurn {
    state: AgentTurnState,
}

impl AgentTurn {
    pub(crate) fn new() -> Self {
        Self {
            state: AgentTurnState::ModelRequest,
        }
    }

    pub(crate) fn phase(&self) -> AgentTurnPhase {
        match self.state {
            AgentTurnState::ModelRequest => AgentTurnPhase::ModelRequest,
            AgentTurnState::ModelStreaming { .. } => AgentTurnPhase::ModelStreaming,
            AgentTurnState::ToolPlanning { .. } => AgentTurnPhase::ToolPlanning,
            AgentTurnState::ToolExecution { .. } | AgentTurnState::ToolExecutionInProgress => {
                AgentTurnPhase::ToolExecution
            }
            AgentTurnState::ToolResultCommit { .. }
            | AgentTurnState::ToolResultCommitInProgress => AgentTurnPhase::ToolResultCommit,
            AgentTurnState::Finished => AgentTurnPhase::Finished,
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        matches!(self.state, AgentTurnState::Finished)
    }

    pub(crate) fn needs_tool_result_commit(&self) -> bool {
        matches!(
            self.state,
            AgentTurnState::ToolPlanning { .. }
                | AgentTurnState::ToolExecution { .. }
                | AgentTurnState::ToolExecutionInProgress
                | AgentTurnState::ToolResultCommit { .. }
                | AgentTurnState::ToolResultCommitInProgress
        )
    }

    pub(crate) fn finish(&mut self) {
        self.state = AgentTurnState::Finished;
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

    pub(crate) fn open_model_stream(&mut self) -> Result<()> {
        ensure_phase(self.phase(), AgentTurnPhase::ModelRequest)?;
        self.state = AgentTurnState::ModelStreaming {
            output_state: OutputStreamState::new(),
        };
        Ok(())
    }

    pub(crate) fn apply_output_delta(
        &mut self,
        content_index: usize,
        delta: OutputContentDelta,
    ) -> Result<()> {
        let AgentTurnState::ModelStreaming { output_state } = &mut self.state else {
            return Err(unexpected_state("model streaming"));
        };

        output_state.apply(OutputStreamEvent::Delta {
            content_index,
            delta,
        })?;
        Ok(())
    }

    pub(crate) fn finish_output(
        &mut self,
        reason: FinishReason,
        usage: Option<Usage>,
    ) -> Result<PendingOutput> {
        let AgentTurnState::ModelStreaming { output_state } = &mut self.state else {
            return Err(unexpected_state("model streaming"));
        };

        let response = output_state
            .apply(OutputStreamEvent::Finished { reason, usage })?
            .ok_or_else(|| Error::protocol("stream ended before finished event"))?;

        Ok(PendingOutput {
            content: into_assistant_content(response.message)?,
            reason: response.reason,
            usage: response.usage,
        })
    }

    pub(crate) fn commit_output(&mut self, output: PendingOutput) -> Result<CommittedOutput> {
        let tool_calls = output
            .content
            .iter()
            .filter_map(|content| match content {
                OutputContent::ToolCall(tool) => Some(tool.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let ends_turn = tool_calls.is_empty();

        self.state = if ends_turn {
            AgentTurnState::Finished
        } else {
            AgentTurnState::ToolPlanning { tool_calls }
        };

        Ok(CommittedOutput {
            content: output.content,
            reason: output.reason,
            usage: output.usage,
        })
    }

    pub(crate) fn tool_calls(&self) -> Result<Vec<ToolCall>> {
        match &self.state {
            AgentTurnState::ToolPlanning { tool_calls } => Ok(tool_calls.clone()),
            _ => Err(unexpected_state("tool planning")),
        }
    }

    pub(crate) fn set_tool_plan(&mut self, plan: Vec<ToolPlanItem>) -> Result<()> {
        ensure_phase(self.phase(), AgentTurnPhase::ToolPlanning)?;
        self.state = AgentTurnState::ToolExecution { plan };
        Ok(())
    }

    pub(crate) fn take_tool_plan(&mut self) -> Result<Vec<ToolPlanItem>> {
        match mem::replace(&mut self.state, AgentTurnState::ToolExecutionInProgress) {
            AgentTurnState::ToolExecution { plan } => Ok(plan),
            state => {
                self.state = state;
                Err(unexpected_state("tool execution"))
            }
        }
    }

    pub(crate) fn abort_pending_tools(&mut self) -> Result<()> {
        self.state = match mem::replace(&mut self.state, AgentTurnState::Finished) {
            AgentTurnState::ToolPlanning { tool_calls } => AgentTurnState::ToolResultCommit {
                completed_tools: aborted_tool_calls(tool_calls),
                finish_after_commit: true,
            },
            AgentTurnState::ToolExecution { plan } => AgentTurnState::ToolResultCommit {
                completed_tools: abort_tool_plan(plan),
                finish_after_commit: true,
            },
            state => {
                self.state = state;
                return Err(unexpected_state("pending tools"));
            }
        };
        Ok(())
    }

    pub(crate) fn set_completed_tools(
        &mut self,
        completed_tools: Vec<(ToolCall, ToolResult)>,
        finish_after_commit: bool,
    ) -> Result<()> {
        ensure_phase(self.phase(), AgentTurnPhase::ToolExecution)?;
        self.state = AgentTurnState::ToolResultCommit {
            completed_tools,
            finish_after_commit,
        };
        Ok(())
    }

    pub(crate) fn take_tool_results_for_commit(&mut self) -> Result<PendingToolResults> {
        match mem::replace(&mut self.state, AgentTurnState::ToolResultCommitInProgress) {
            AgentTurnState::ToolResultCommit {
                completed_tools,
                finish_after_commit,
            } => Ok(PendingToolResults {
                completed_tools,
                finish_after_commit,
            }),
            state => {
                self.state = state;
                Err(unexpected_state("tool result commit"))
            }
        }
    }

    pub(crate) fn finish_tool_result_commit(&mut self, finish_turn: bool) {
        self.state = if finish_turn {
            AgentTurnState::Finished
        } else {
            AgentTurnState::ModelRequest
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentTurnPhase {
    ModelRequest,
    ModelStreaming,
    ToolPlanning,
    ToolExecution,
    ToolResultCommit,
    Finished,
}

pub(crate) struct PendingOutput {
    pub(crate) content: Vec<OutputContent>,
    pub(crate) reason: FinishReason,
    pub(crate) usage: Option<Usage>,
}

pub(crate) struct CommittedOutput {
    pub(crate) content: Vec<OutputContent>,
    pub(crate) reason: FinishReason,
    pub(crate) usage: Option<Usage>,
}

pub(crate) struct PendingToolResults {
    pub(crate) completed_tools: Vec<(ToolCall, ToolResult)>,
    pub(crate) finish_after_commit: bool,
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

enum AgentTurnState {
    ModelRequest,
    ModelStreaming {
        output_state: OutputStreamState,
    },
    ToolPlanning {
        tool_calls: Vec<ToolCall>,
    },
    ToolExecution {
        plan: Vec<ToolPlanItem>,
    },
    ToolExecutionInProgress,
    ToolResultCommit {
        completed_tools: Vec<(ToolCall, ToolResult)>,
        finish_after_commit: bool,
    },
    ToolResultCommitInProgress,
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

fn aborted_tool_calls(tool_calls: Vec<ToolCall>) -> Vec<(ToolCall, ToolResult)> {
    tool_calls
        .into_iter()
        .map(|tool| {
            let result = aborted_tool_result(&tool);
            (tool, result)
        })
        .collect()
}

fn abort_tool_plan(plan: Vec<ToolPlanItem>) -> Vec<(ToolCall, ToolResult)> {
    plan.into_iter()
        .map(|item| {
            let result = item
                .completed_result
                .unwrap_or_else(|| aborted_tool_result(&item.tool));
            (item.tool, result)
        })
        .collect()
}

fn into_assistant_content(message: Message) -> Result<Vec<OutputContent>> {
    match message {
        Message::Assistant(content) => Ok(content),
        other => Err(Error::protocol(format!(
            "expected assistant message, got {other:?}"
        ))),
    }
}

fn ensure_phase(actual: AgentTurnPhase, expected: AgentTurnPhase) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(unexpected_state(match expected {
            AgentTurnPhase::ModelRequest => "model request",
            AgentTurnPhase::ModelStreaming => "model streaming",
            AgentTurnPhase::ToolPlanning => "tool planning",
            AgentTurnPhase::ToolExecution => "tool execution",
            AgentTurnPhase::ToolResultCommit => "tool result commit",
            AgentTurnPhase::Finished => "finished",
        }))
    }
}

fn unexpected_state(expected: &str) -> Error {
    Error::protocol(format!("expected {expected} turn state"))
}
