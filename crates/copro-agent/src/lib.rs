pub use copro_api::async_trait;
pub use copro_api::message::{ToolCall, ToolCallId, ToolResult};
pub use tokio_util::sync::CancellationToken;

mod agent;
mod cancel;
mod context;
mod event;
mod tools;
mod turn;

pub use agent::Agent;
pub use context::AgentContext;
pub use event::{AgentEvent, AgentStream};
pub use tools::{ToolExecutionPolicy, ToolRouter};
pub use turn::{
    AgentAction, AgentCheckpoint, AgentControl, AgentControlKind, AgentInterruptReason,
    AgentOutcome, AgentStep, AgentStepId, AgentStepReport, AgentTurnHandle, AgentTurnState,
    ToolResultReplacement,
};
