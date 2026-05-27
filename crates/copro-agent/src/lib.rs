pub use copro_core::async_trait;

pub mod runtime;

mod agent;
mod event;
mod hook;

pub use agent::Agent;
pub use event::{AgentEvent, AgentStream};
pub use hook::{AgentHook, ToolDecision, ToolExecuteContext, ToolResultContext};
