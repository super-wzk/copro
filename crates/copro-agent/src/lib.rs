pub use copro_api::async_trait;
pub use copro_api::message::{ToolCall, ToolCallId, ToolResult};
pub use tokio_util::sync::CancellationToken;

pub mod runtime;

mod agent;
mod context;
mod event;
mod hook;
mod run;
mod tools;
mod turn;

pub use agent::Agent;
pub use event::{AgentEvent, AgentStream};
pub use hook::{AgentHook, AgentHooks, ToolCallDecision};
pub use run::{
    AgentAction, AgentControl, AgentInterruptReason, AgentOutcome, AgentRunId, AgentRunState,
    AgentStep, AgentStepId, AgentTurnId,
};
pub use runtime::StopSignal;
pub use tools::{ToolExecutionPolicy, ToolRouter};
