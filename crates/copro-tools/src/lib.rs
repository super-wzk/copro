use copro_agent::{ToolProvider, async_trait};
use copro_api::error::Result;
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_api::tool::{ErasedTool, ToolDefinition};
use serde_json::Value;
use std::sync::Arc;

/// Tool provider backed by in-process [`ErasedTool`] implementations.
#[derive(Default, Clone)]
pub struct LocalToolProvider {
    tools: Vec<Arc<dyn ErasedTool>>,
}

impl LocalToolProvider {
    pub fn new(tools: Vec<Arc<dyn ErasedTool>>) -> Self {
        Self { tools }
    }

    pub fn push(&mut self, tool: Arc<dyn ErasedTool>) {
        self.tools.push(tool);
    }

    pub fn add_tool<T>(&mut self, tool: T)
    where
        T: ErasedTool + 'static,
    {
        self.tools.push(Arc::new(tool));
    }
}

#[async_trait]
impl ToolProvider for LocalToolProvider {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        Ok(self.tools.iter().map(|tool| tool.definition()).collect())
    }

    async fn execute(&self, call: ToolCall) -> Result<ToolResult> {
        let ToolCall {
            id,
            name,
            arguments,
        } = call;

        let Some(tool) = self
            .tools
            .iter()
            .find(|tool| tool.definition().name == name)
        else {
            return Ok(ToolResult {
                call_id: id,
                name: name.clone(),
                status: ToolResultStatus::Error,
                content: vec![InputContent::Text(format!("unknown tool: {name}"))],
            });
        };

        let result = match tool.call_content(Value::Object(arguments)).await {
            Ok(content) => ToolResult {
                call_id: id,
                name,
                status: ToolResultStatus::Success,
                content,
            },
            Err(error) => ToolResult {
                call_id: id,
                name,
                status: ToolResultStatus::Error,
                content: vec![InputContent::Text(error)],
            },
        };

        Ok(result)
    }
}
