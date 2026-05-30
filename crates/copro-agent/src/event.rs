use crate::run::{AgentOutcome, AgentRunId, AgentStep, AgentStepId, AgentTurnId};
use copro_api::error::Result;
use copro_api::message::{OutputContent, ToolCall, ToolResult};
use copro_api::response::{FinishReason, Usage};
use copro_api::stream::OutputContentDelta;
use std::pin::Pin;

/// Core step-level events emitted by schedulable agent runs.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    RunStarted {
        run_id: AgentRunId,
    },
    RunPaused {
        run_id: AgentRunId,
        at: AgentStepId,
    },
    RunResumed {
        run_id: AgentRunId,
        at: AgentStepId,
    },
    RunPreempted {
        run_id: AgentRunId,
        at: AgentStepId,
    },
    RunRecovering {
        run_id: AgentRunId,
        after: AgentStepId,
    },
    RunFinished {
        run_id: AgentRunId,
    },
    RunAborted {
        run_id: AgentRunId,
    },

    TurnStarted {
        run_id: AgentRunId,
        turn_id: AgentTurnId,
    },
    TurnFinished {
        run_id: AgentRunId,
        turn_id: AgentTurnId,
    },

    StepReady {
        step: AgentStep,
    },
    StepStarted {
        step: AgentStep,
    },
    StepCompleted {
        step: AgentStep,
        outcome: AgentOutcome,
    },
    ControlRequired {
        step: AgentStep,
        outcome: AgentOutcome,
    },

    ModelDelta {
        step_id: AgentStepId,
        content_index: usize,
        delta: OutputContentDelta,
    },
    AssistantCommitted {
        step_id: AgentStepId,
        message_index: usize,
        content: Vec<OutputContent>,
        reason: FinishReason,
        usage: Option<Usage>,
    },
    ToolStarted {
        step_id: AgentStepId,
        tool: ToolCall,
    },
    ToolResultCommitted {
        step_id: AgentStepId,
        message_index: usize,
        tool: ToolCall,
        result: ToolResult,
    },
}

/// A stream of core [`AgentEvent`]s produced by an agent run.
pub type AgentStream = Pin<Box<dyn futures_util::Stream<Item = Result<AgentEvent>> + Send>>;
