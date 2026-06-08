use super::AgentTurnConfig;
use crate::history::AgentHistory;
use crate::tools::ToolRouter;
use copro_api::message::InputMessage;
use copro_api::stream::Model;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Clone)]
pub(crate) struct PendingTurnInputs {
    inner: Arc<Mutex<VecDeque<InputMessage>>>,
}

impl PendingTurnInputs {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub(crate) fn push(&self, input: InputMessage) {
        self.inner
            .lock()
            .expect("pending turn input mutex poisoned")
            .push_back(input);
    }

    pub(crate) fn drain(&self) -> Vec<InputMessage> {
        self.inner
            .lock()
            .expect("pending turn input mutex poisoned")
            .drain(..)
            .collect()
    }
}

pub(crate) struct AgentTurnResources {
    pub(crate) model: Arc<dyn Model>,
    pub(crate) tools: Arc<dyn ToolRouter>,
    pub(crate) history: AgentHistory,
    pub(crate) config: AgentTurnConfig,
    pub(crate) pending_inputs: PendingTurnInputs,
}

impl AgentTurnResources {
    pub(crate) fn new(
        history: AgentHistory,
        config: AgentTurnConfig,
        model: Arc<dyn Model>,
        tools: Arc<dyn ToolRouter>,
        pending_inputs: PendingTurnInputs,
    ) -> Self {
        Self {
            model,
            tools,
            history,
            config,
            pending_inputs,
        }
    }

    pub(crate) fn into_history(self) -> AgentHistory {
        self.history
    }
}
