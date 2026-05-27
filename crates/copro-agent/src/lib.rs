pub use copro_core::async_trait;
pub use copro_core::message::{ToolCall, ToolResult};

pub mod runtime;

mod agent;
mod event;
mod hook;
mod tools;

pub use agent::Agent;
pub use event::{AgentEvent, AgentStream};
pub use hook::{AgentHook, ToolDecision};
pub use tools::ToolProvider;
