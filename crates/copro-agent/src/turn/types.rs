use crate::tools::ToolExecutionPolicy;
use copro_api::message::{OutputContent, ToolCall, ToolResult};
use copro_api::request::GenerateRequest;
use copro_api::response::{FinishReason, Usage};
use copro_api::stream::OutputContentDelta;
use copro_api::tool::ToolDefinition;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentStepId {
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
pub enum AgentTurnState {
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
