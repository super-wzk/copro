use copro_core::error::Result;
use copro_core::message::{InputContent, Message, ToolResultStatus};
use copro_core::request::GenerateRequest;
use serde_json::{Map, Value};

/// Hook points that can inspect or modify agent execution.
pub trait AgentHook: Send + Sync {
    fn before_request(&self, _request: &mut GenerateRequest) -> Result<()> {
        Ok(())
    }

    fn before_tool_execute(&self, _tool: &mut ToolExecuteContext) -> Result<ToolDecision> {
        Ok(ToolDecision::Allow)
    }

    fn after_tool_result(&self, _result: &mut ToolResultContext) -> Result<()> {
        Ok(())
    }

    fn on_output_finished(&self, _message: &mut Message) -> Result<()> {
        Ok(())
    }
}

/// Decision returned by [`AgentHook::before_tool_execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    Reject { reason: String },
}

/// Tool execution context passed to [`AgentHook::before_tool_execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecuteContext {
    pub call_id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
}

/// Tool result context passed to [`AgentHook::after_tool_result`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultContext {
    pub call_id: String,
    pub name: String,
    pub status: ToolResultStatus,
    pub content: Vec<InputContent>,
}
