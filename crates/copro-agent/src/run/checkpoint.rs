use super::types::{AgentOutcome, AgentRunState, AgentStep, AgentStepId};
use crate::event::AgentEvent;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentStepReport {
    pub step: AgentStep,
    pub outcome: AgentOutcome,
    pub state: AgentRunState,
    pub events: Vec<AgentEvent>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentCheckpoint {
    Basic(AgentStepReport),
    RequestBuilt(AgentStepReport),
    ModelDelta(AgentStepReport),
    AssistantOutput(AgentStepReport),
    ToolPlanned(AgentStepReport),
    ToolRejected(AgentStepReport),
    ToolResult(AgentStepReport),
}

impl AgentCheckpoint {
    pub(crate) fn from_report(report: AgentStepReport) -> Self {
        match &report.outcome {
            AgentOutcome::RequestBuilt(_) => Self::RequestBuilt(report),
            AgentOutcome::ModelDelta { .. } => Self::ModelDelta(report),
            AgentOutcome::ModelOutputFinished { .. } => Self::AssistantOutput(report),
            AgentOutcome::ToolPlanned { .. } => Self::ToolPlanned(report),
            AgentOutcome::ToolRejected { .. } => Self::ToolRejected(report),
            AgentOutcome::ToolResultCommitted { .. } => Self::ToolResult(report),
            _ => Self::Basic(report),
        }
    }

    pub fn report(&self) -> &AgentStepReport {
        match self {
            Self::Basic(report)
            | Self::RequestBuilt(report)
            | Self::ModelDelta(report)
            | Self::AssistantOutput(report)
            | Self::ToolPlanned(report)
            | Self::ToolRejected(report)
            | Self::ToolResult(report) => report,
        }
    }

    pub fn step(&self) -> &AgentStep {
        &self.report().step
    }

    pub fn step_id(&self) -> AgentStepId {
        self.step().id
    }

    pub fn state(&self) -> &AgentRunState {
        &self.report().state
    }

    pub fn events(&self) -> &[AgentEvent] {
        &self.report().events
    }

    pub fn pending_outcome(&self) -> &AgentOutcome {
        &self.report().outcome
    }

    pub fn into_report(self) -> AgentStepReport {
        match self {
            Self::Basic(report)
            | Self::RequestBuilt(report)
            | Self::ModelDelta(report)
            | Self::AssistantOutput(report)
            | Self::ToolPlanned(report)
            | Self::ToolRejected(report)
            | Self::ToolResult(report) => report,
        }
    }
}
