use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{ToolCall, ToolResult};
use copro_api::tool::ToolDefinition;

/// Routes model-callable tool definitions and executes tool calls.
#[async_trait]
pub trait ToolRouter: Send + Sync {
    /// Return tool definitions available to the model for the next request.
    async fn definitions(&self) -> Result<Vec<ToolDefinition>>;

    /// Execute one model-requested tool call.
    async fn execute(&self, call: ToolCall) -> Result<ToolResult>;
}
