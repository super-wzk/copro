pub use copro_api::async_trait;
pub use copro_api::message::{ToolCall, ToolResult};

pub mod runtime;

mod agent;
mod event;
mod hook;
mod tools;

pub use agent::Agent;
pub use event::{AgentEvent, AgentStream};
pub use hook::{AgentHook, AgentHooks, ToolCallDecision};
pub use runtime::StopSignal;
pub use tools::{ToolExecutionPolicy, ToolRouter};
