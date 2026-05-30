pub use copro_api::async_trait;
pub use copro_api::message::{ToolCall, ToolCallId, ToolResult};
pub use tokio_util::sync::CancellationToken;

pub mod runtime;

mod agent;
mod context;
mod event;
mod run;
mod tools;
mod turn;

pub use agent::Agent;
pub use event::{AgentEvent, AgentStream};
pub use run::{
    AgentAction, AgentControl, AgentControlDecision, AgentControlKind, AgentControlPoint,
    AgentInterruptReason, AgentOutcome, AgentRunHandle, AgentRunId, AgentRunState, AgentStep,
    AgentStepId, AgentStepReport, AgentTurnId, AssistantOutputControlPoint, BasicControlPoint,
    ModelDeltaControlPoint, RequestControlPoint, ToolCallControlPoint, ToolResultControlPoint,
    ToolResultReplacement,
};
pub use runtime::StopSignal;
pub use tools::{ToolExecutionPolicy, ToolRouter};
