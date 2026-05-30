use crate::run::{AgentOutcome, AgentRunId, AgentStep, AgentStepId};
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

impl AgentEvent {
    pub fn run_id(&self) -> AgentRunId {
        match self {
            Self::RunStarted { run_id }
            | Self::RunPaused { run_id, .. }
            | Self::RunResumed { run_id, .. }
            | Self::RunPreempted { run_id, .. }
            | Self::RunRecovering { run_id, .. }
            | Self::RunFinished { run_id }
            | Self::RunAborted { run_id } => *run_id,
            Self::StepReady { step }
            | Self::StepStarted { step }
            | Self::StepCompleted { step, .. }
            | Self::ControlRequired { step, .. } => step.id.run_id,
            Self::ModelDelta { step_id, .. }
            | Self::AssistantCommitted { step_id, .. }
            | Self::ToolStarted { step_id, .. }
            | Self::ToolResultCommitted { step_id, .. } => step_id.run_id,
        }
    }

    pub fn step_id(&self) -> Option<AgentStepId> {
        match self {
            Self::RunStarted { .. } | Self::RunFinished { .. } | Self::RunAborted { .. } => None,
            Self::RunPaused { at, .. }
            | Self::RunResumed { at, .. }
            | Self::RunPreempted { at, .. } => Some(*at),
            Self::RunRecovering { after, .. } => Some(*after),
            Self::StepReady { step }
            | Self::StepStarted { step }
            | Self::StepCompleted { step, .. }
            | Self::ControlRequired { step, .. } => Some(step.id),
            Self::ModelDelta { step_id, .. }
            | Self::AssistantCommitted { step_id, .. }
            | Self::ToolStarted { step_id, .. }
            | Self::ToolResultCommitted { step_id, .. } => Some(*step_id),
        }
    }
}

/// A stream of core [`AgentEvent`]s produced by an agent run.
pub type AgentStream = Pin<Box<dyn futures_util::Stream<Item = Result<AgentEvent>> + Send>>;

#[cfg(test)]
mod tests {
    use super::AgentEvent;
    use crate::run::{AgentAction, AgentRunId, AgentStep, AgentStepId};
    use copro_api::stream::OutputContentDelta;

    #[test]
    fn event_accessors_return_run_and_step_ids() {
        let run_id = AgentRunId(7);
        let step_id = AgentStepId { run_id, tick: 3 };
        let step = AgentStep {
            id: step_id,
            action: AgentAction::ReadModelStream,
        };

        assert_eq!(AgentEvent::RunStarted { run_id }.run_id(), run_id);
        assert_eq!(AgentEvent::RunStarted { run_id }.step_id(), None);

        let step_event = AgentEvent::StepStarted { step };
        assert_eq!(step_event.run_id(), run_id);
        assert_eq!(step_event.step_id(), Some(step_id));

        let domain_event = AgentEvent::ModelDelta {
            step_id,
            content_index: 0,
            delta: OutputContentDelta::Text("hi".to_string()),
        };
        assert_eq!(domain_event.run_id(), run_id);
        assert_eq!(domain_event.step_id(), Some(step_id));

        let boundary_event = AgentEvent::RunPaused {
            run_id,
            at: step_id,
        };
        assert_eq!(boundary_event.run_id(), run_id);
        assert_eq!(boundary_event.step_id(), Some(step_id));
    }
}
