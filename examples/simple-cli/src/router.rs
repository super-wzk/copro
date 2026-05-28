use copro_agent::{ToolRouter, async_trait};
use copro_api::error::Result;
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_api::tool::ToolDefinition;
use std::sync::Arc;

pub struct CompositeToolRouter {
    routers: Vec<Arc<dyn ToolRouter>>,
}

impl CompositeToolRouter {
    pub fn new(routers: Vec<Arc<dyn ToolRouter>>) -> Self {
        Self { routers }
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

    async fn execute(&self, call: ToolCall) -> Result<ToolResult> {
        for router in &self.routers {
            if router
                .definitions()
                .await?
                .iter()
                .any(|definition| definition.name == call.name)
            {
                return router.execute(call).await;
            }
        }

        Ok(ToolResult {
            call_id: call.id,
            name: call.name.clone(),
            status: ToolResultStatus::Error,
            content: vec![InputContent::Text(format!("unknown tool: {}", call.name))],
        })
    }
}
