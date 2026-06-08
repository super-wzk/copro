use super::handle::{AgentTurnCompletion, AgentTurnHandle};
use super::{AgentTurn, AgentTurnConfig, AgentTurnResources, PendingTurnInputs};
use crate::cancel::TurnCancellation;
use crate::history::AgentHistory;
use crate::tools::ToolRouter;
use copro_api::stream::Model;
use std::sync::Arc;
use tokio::sync::mpsc;

const EVENT_BUFFER: usize = 16;

pub fn start_turn(
    history: AgentHistory,
    config: AgentTurnConfig,
    model: Arc<dyn Model>,
    tools: Arc<dyn ToolRouter>,
) -> AgentTurnHandle {
    let (events, rx) = mpsc::channel(EVENT_BUFFER);
    let cancellation = TurnCancellation::new();
    let completion = AgentTurnCompletion::new();
    let pending_inputs = PendingTurnInputs::new();
    let handle = AgentTurnHandle::new(
        rx,
        cancellation.clone(),
        completion.clone(),
        pending_inputs.clone(),
    );

    tokio::spawn(async move {
        let mut resources = AgentTurnResources::new(history, config, model, tools, pending_inputs);
        AgentTurn::new(&mut resources, cancellation)
            .execute(events)
            .await;
        completion.complete(resources.into_history()).await;
    });

    handle
}
