use copro_api::message::{InputContent, OutputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_api::request::GenerateRequest;
use copro_api::stream::OutputContentDelta;
use tokio::sync::oneshot;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentControl {
    Continue,
    Pause,
    FinishRun,
    AbortRun,
    ReplaceRequest(GenerateRequest),
    ReplaceModelDelta(OutputContentDelta),
    DropModelDelta,
    ReplaceAssistantOutput(Vec<OutputContent>),
    ReplaceToolCall(ToolCall),
    RejectToolCall { reason: String },
    ReplaceToolResult(ToolResult),
    ReplaceToolResultContent(ToolResultReplacement),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultReplacement {
    pub status: ToolResultStatus,
    pub content: Vec<InputContent>,
}

pub(crate) enum AgentControlSignal {
    Control(AgentControl),
    Pause { resume: oneshot::Receiver<()> },
    Preempt,
}

impl AgentControlSignal {
    pub(crate) fn continue_run() -> Self {
        Self::Control(AgentControl::Continue)
    }

    pub(crate) fn preempt() -> Self {
        Self::Preempt
    }

    pub(crate) fn kind(&self) -> AgentControlKind {
        match self {
            AgentControlSignal::Control(control) => control.kind(),
            AgentControlSignal::Pause { .. } => AgentControlKind::Pause,
            AgentControlSignal::Preempt => AgentControlKind::AbortRun,
        }
    }
}

impl AgentControl {
    pub fn kind(&self) -> AgentControlKind {
        match self {
            AgentControl::Continue => AgentControlKind::Continue,
            AgentControl::Pause => AgentControlKind::Pause,
            AgentControl::FinishRun => AgentControlKind::FinishRun,
            AgentControl::AbortRun => AgentControlKind::AbortRun,
            AgentControl::ReplaceRequest(_) => AgentControlKind::ReplaceRequest,
            AgentControl::ReplaceModelDelta(_) => AgentControlKind::ReplaceModelDelta,
            AgentControl::DropModelDelta => AgentControlKind::DropModelDelta,
            AgentControl::ReplaceAssistantOutput(_) => AgentControlKind::ReplaceAssistantOutput,
            AgentControl::ReplaceToolCall(_) => AgentControlKind::ReplaceToolCall,
            AgentControl::RejectToolCall { .. } => AgentControlKind::RejectToolCall,
            AgentControl::ReplaceToolResult(_) | AgentControl::ReplaceToolResultContent(_) => {
                AgentControlKind::ReplaceToolResult
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentControlKind {
    Continue,
    Pause,
    FinishRun,
    AbortRun,
    ReplaceRequest,
    ReplaceModelDelta,
    DropModelDelta,
    ReplaceAssistantOutput,
    ReplaceToolCall,
    RejectToolCall,
    ReplaceToolResult,
}
