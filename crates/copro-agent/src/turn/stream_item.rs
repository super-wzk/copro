use super::{AgentControlSignal, AgentOutcome, AgentStep};
use crate::event::AgentEvent;
use tokio::sync::oneshot;

pub(crate) enum AgentStreamItem {
    Event(Box<AgentEvent>),
    ControlRequired {
        step: Box<AgentStep>,
        outcome: Box<AgentOutcome>,
        reply: oneshot::Sender<AgentControlSignal>,
    },
    Error(copro_api::error::Error),
}
