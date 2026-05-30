pub use copro_api::async_trait;
pub use copro_api::message::{InputMessage, OutputMessage, ToolCall, ToolCallId, ToolResult};
pub use tokio_util::sync::CancellationToken;

mod cancel;
mod event;
mod history;
mod tools;
mod turn;

pub use event::{AgentEvent, AgentStream};
pub use history::AgentHistory;
pub use tools::{ToolExecutionPolicy, ToolRouter};
pub use turn::{
    AgentAction, AgentCheckpoint, AgentControl, AgentControlKind, AgentInterruptReason,
    AgentOutcome, AgentStep, AgentStepId, AgentStepReport, AgentTurnConfig, AgentTurnHandle,
    AgentTurnState, ToolResultReplacement, start_turn,
};
