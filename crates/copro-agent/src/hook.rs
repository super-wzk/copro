use copro_core::async_trait;
use copro_core::error::Result;
use copro_core::message::{OutputContent, ToolCall, ToolResult};
use copro_core::request::GenerateRequest;

/// Hook points that can inspect or modify agent execution.
#[async_trait]
pub trait AgentHook: Send + Sync {
    async fn before_request(&self, _request: &mut GenerateRequest) -> Result<()> {
        Ok(())
    }

    async fn before_tool_execute(&self, _tool: &mut ToolCall) -> Result<ToolDecision> {
        Ok(ToolDecision::Allow)
    }

    async fn after_tool_result(&self, _result: &mut ToolResult) -> Result<()> {
        Ok(())
    }

    async fn on_output_finished(&self, _content: &mut Vec<OutputContent>) -> Result<()> {
        Ok(())
    }
}

/// Decision returned by [`AgentHook::before_tool_execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    Reject { reason: String },
}
