use super::AgentTurnConfig;
use crate::history::AgentHistory;
use crate::tools::ToolRouter;
use copro_api::stream::Model;
use std::sync::Arc;

pub(crate) struct AgentTurnResources {
    pub(crate) model: Arc<dyn Model>,
    pub(crate) tools: Arc<dyn ToolRouter>,
    pub(crate) history: AgentHistory,
    pub(crate) config: AgentTurnConfig,
}

impl AgentTurnResources {
    pub(crate) fn new(
        history: AgentHistory,
        config: AgentTurnConfig,
        model: Arc<dyn Model>,
        tools: Arc<dyn ToolRouter>,
    ) -> Self {
        Self {
            model,
            tools,
            history,
            config,
        }
    }

    pub(crate) fn into_history(self) -> AgentHistory {
        self.history
    }
}
