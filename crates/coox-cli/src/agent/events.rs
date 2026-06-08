use crate::tui::state::AppState;
use copro_agent::AgentEvent;

pub fn apply_agent_event(event: AgentEvent, state: &mut AppState) {
    match event {
        AgentEvent::TurnStarted | AgentEvent::TurnFinished | AgentEvent::TurnAborted => {}
        AgentEvent::InputCommitted { input, .. } => {
            state.push_input(input);
        }
        AgentEvent::ModelDelta {
            content_index,
            delta,
            ..
        } => {
            state.apply_delta_at(content_index, delta);
        }
        AgentEvent::ToolStarted { .. } => {}
        AgentEvent::ToolResultCommitted { result, .. } => {
            state.apply_tool_result(result);
        }
        AgentEvent::AssistantCommitted { .. }
        | AgentEvent::StepReady { .. }
        | AgentEvent::StepStarted { .. }
        | AgentEvent::StepCompleted { .. }
        | AgentEvent::ControlRequired { .. }
        | AgentEvent::TurnPaused { .. }
        | AgentEvent::TurnResumed { .. }
        | AgentEvent::TurnPreempted { .. }
        | AgentEvent::TurnRecovering { .. } => {}
    }
}

pub fn apply_runtime_error(message: impl Into<String>, state: &mut AppState) {
    state.push_error(message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use copro_api::stream::OutputContentDelta;

    #[test]
    fn model_delta_updates_app_state_without_mapping_protocol() {
        let mut state = AppState::default();

        apply_agent_event(
            AgentEvent::ModelDelta {
                step_id: copro_agent::AgentStepId { tick: 1 },
                content_index: 0,
                delta: OutputContentDelta::Text("hello".to_string()),
            },
            &mut state,
        );

        assert_eq!(state.blocks().len(), 1);
    }

    #[test]
    fn runtime_error_appends_error_block() {
        let mut state = AppState::default();

        apply_runtime_error("client error: missing api key", &mut state);

        assert_eq!(state.blocks().len(), 1);
    }
}
