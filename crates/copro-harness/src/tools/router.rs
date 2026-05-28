use super::tool::ErasedTool;
use copro_agent::{CancellationToken, ToolExecutionPolicy, ToolRouter};
use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_api::tool::ToolDefinition;
use serde_json::Value;
use std::sync::Arc;

/// Tool router backed by in-process [`ErasedTool`] implementations.
#[derive(Default, Clone)]
pub struct LocalToolRouter {
    tools: Vec<Arc<dyn ErasedTool>>,
}

impl LocalToolRouter {
    pub fn new(tools: Vec<Arc<dyn ErasedTool>>) -> Self {
        Self { tools }
    }

    fn tool_by_name(&self, name: &str) -> Option<&Arc<dyn ErasedTool>> {
        self.tools
            .iter()
            .find(|tool| tool.definition().name == name)
    }
}

#[async_trait]
impl ToolRouter for LocalToolRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        Ok(self.tools.iter().map(|tool| tool.definition()).collect())
    }

    async fn execute(&self, call: ToolCall, cancel: CancellationToken) -> Result<ToolResult> {
        let ToolCall {
            id,
            name,
            arguments,
        } = call;

        let Some(tool) = self.tool_by_name(&name) else {
            return Ok(ToolResult {
                call_id: id,
                name: name.clone(),
                status: ToolResultStatus::Error,
                content: vec![InputContent::Text(format!("unknown tool: {name}"))],
            });
        };

        let result = match tool.call_content(Value::Object(arguments), cancel).await {
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

    async fn execution_policy(&self, call: &ToolCall) -> Result<ToolExecutionPolicy> {
        Ok(self
            .tool_by_name(&call.name)
            .map(|tool| tool.execution_policy())
            .unwrap_or(ToolExecutionPolicy::Serial))
    }
}

/// Tool router that exposes and delegates to multiple child routers.
#[derive(Default, Clone)]
pub struct CompositeToolRouter {
    routers: Vec<Arc<dyn ToolRouter>>,
}

impl CompositeToolRouter {
    pub fn new(routers: Vec<Arc<dyn ToolRouter>>) -> Self {
        Self { routers }
    }

    async fn router_for(&self, name: &str) -> Result<Option<Arc<dyn ToolRouter>>> {
        for router in &self.routers {
            if router
                .definitions()
                .await?
                .iter()
                .any(|definition| definition.name == name)
            {
                return Ok(Some(Arc::clone(router)));
            }
        }

        Ok(None)
    }
}

#[async_trait]
impl ToolRouter for CompositeToolRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        let mut definitions = Vec::new();
        for router in &self.routers {
            definitions.extend(router.definitions().await?);
        }
        Ok(definitions)
    }

    async fn execute(&self, call: ToolCall, cancel: CancellationToken) -> Result<ToolResult> {
        let name = call.name.clone();
        if let Some(router) = self.router_for(&name).await? {
            return router.execute(call, cancel).await;
        }

        Ok(ToolResult {
            call_id: call.id,
            name: call.name.clone(),
            status: ToolResultStatus::Error,
            content: vec![InputContent::Text(format!("unknown tool: {}", call.name))],
        })
    }

    async fn execution_policy(&self, call: &ToolCall) -> Result<ToolExecutionPolicy> {
        if let Some(router) = self.router_for(&call.name).await? {
            return router.execution_policy(call).await;
        }

        Ok(ToolExecutionPolicy::Serial)
    }
}
