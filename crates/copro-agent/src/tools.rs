use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{ToolCall, ToolResult};
use copro_api::tool::ToolDefinition;

/// Declares how a tool call may be scheduled relative to other tool calls in
/// the same model output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ToolExecutionPolicy {
    /// Execute this call as an exclusive barrier.
    #[default]
    Serial,
    /// This call may overlap with other parallel tool calls in the same batch.
    Parallel,
}

/// Routes model-callable tool definitions and executes tool calls.
#[async_trait]
pub trait ToolRouter: Send + Sync {
    /// Return tool definitions available to the model for the next request.
    async fn definitions(&self) -> Result<Vec<ToolDefinition>>;

    /// Execute one model-requested tool call.
    async fn execute(&self, call: ToolCall) -> Result<ToolResult>;

    /// Return the execution policy for one model-requested tool call.
    async fn execution_policy(&self, _call: &ToolCall) -> Result<ToolExecutionPolicy> {
        Ok(ToolExecutionPolicy::Serial)
    }
}
