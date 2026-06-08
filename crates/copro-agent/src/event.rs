use crate::turn::{AgentOutcome, AgentStep, AgentStepId};
use copro_api::error::Result;
use copro_api::message::{InputMessage, OutputContent, ToolCall, ToolResult};
use copro_api::response::{FinishReason, Usage};
use copro_api::stream::OutputContentDelta;
use std::pin::Pin;

/// Core step-level events emitted by schedulable agent turns.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TurnStarted,
    TurnPaused {
        at: AgentStepId,
    },
    TurnResumed {
        at: AgentStepId,
    },
    TurnPreempted {
        at: AgentStepId,
    },
    TurnRecovering {
        after: AgentStepId,
    },
    TurnFinished,
    TurnAborted,

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

    InputCommitted {
        step_id: AgentStepId,
        message_index: usize,
        input: InputMessage,
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
    pub fn step_id(&self) -> Option<AgentStepId> {
        match self {
            Self::TurnStarted | Self::TurnFinished | Self::TurnAborted => None,
            Self::TurnPaused { at, .. }
            | Self::TurnResumed { at, .. }
            | Self::TurnPreempted { at, .. } => Some(*at),
            Self::TurnRecovering { after, .. } => Some(*after),
            Self::StepReady { step }
            | Self::StepStarted { step }
            | Self::StepCompleted { step, .. }
            | Self::ControlRequired { step, .. } => Some(step.id),
            Self::InputCommitted { step_id, .. }
            | Self::ModelDelta { step_id, .. }
            | Self::AssistantCommitted { step_id, .. }
            | Self::ToolStarted { step_id, .. }
            | Self::ToolResultCommitted { step_id, .. } => Some(*step_id),
        }
    }
}

/// A stream of core [`AgentEvent`]s produced by an agent turn.
pub type AgentStream = Pin<Box<dyn futures_util::Stream<Item = Result<AgentEvent>> + Send>>;

#[cfg(test)]
mod tests {
    use super::AgentEvent;
    use crate::turn::{AgentAction, AgentStep, AgentStepId};
    use copro_api::stream::OutputContentDelta;

    #[test]
    fn event_accessor_returns_step_id() {
        let step_id = AgentStepId { tick: 3 };
        let step = AgentStep {
            id: step_id,
            action: AgentAction::ReadModelStream,
        };

        assert_eq!(AgentEvent::TurnStarted.step_id(), None);

        let step_event = AgentEvent::StepStarted { step };
        assert_eq!(step_event.step_id(), Some(step_id));

        let domain_event = AgentEvent::ModelDelta {
            step_id,
            content_index: 0,
            delta: OutputContentDelta::Text("hi".to_string()),
        };
        assert_eq!(domain_event.step_id(), Some(step_id));

        let boundary_event = AgentEvent::TurnPaused { at: step_id };
        assert_eq!(boundary_event.step_id(), Some(step_id));
    }
}
