use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{OutputContent, ToolCall, ToolResult};
use copro_api::request::GenerateRequest;

/// Hook points that can inspect or modify agent execution.
#[async_trait]
pub trait AgentHook: Send + Sync {
    async fn before_request(&self, _request: &mut GenerateRequest) -> Result<()> {
        Ok(())
    }

    async fn before_tool_call(&self, _tool: &mut ToolCall) -> Result<ToolDecision> {
        Ok(ToolDecision::Allow)
    }

    async fn after_tool_call(&self, _tool: &ToolCall, _result: &mut ToolResult) -> Result<()> {
        Ok(())
    }

    async fn before_output_commit(&self, _content: &mut Vec<OutputContent>) -> Result<()> {
        Ok(())
    }

    async fn after_output_commit(&self, _content: &[OutputContent]) -> Result<()> {
        Ok(())
    }
}

/// Decision returned by [`AgentHook::before_tool_call`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    Reject { reason: String },
}
