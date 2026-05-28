use super::output::ToolOutput;
use copro_agent::ToolExecutionPolicy;
use copro_api::async_trait;
use copro_api::message::InputContent;
use copro_api::tool::ToolDefinition;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

#[async_trait]
pub trait Tool: Send + Sync {
    type Input: DeserializeOwned + JsonSchema + Send;
    type Output: ToolOutput + Send;

    fn name(&self) -> &str;
    fn description(&self) -> &str;

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Serial
    }

    async fn call(&self, input: Self::Input) -> Result<Self::Output, String>;
}

#[async_trait]
pub trait ErasedTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Serial
    }

    async fn call_content(&self, args: Value) -> Result<Vec<InputContent>, String>;
}

#[async_trait]
impl<T: Tool> ErasedTool for T {
    fn definition(&self) -> ToolDefinition {
        let schema = schemars::schema_for!(T::Input);
        ToolDefinition {
            name: Tool::name(self).to_string(),
            description: Tool::description(self).to_string(),
            parameters: serde_json::to_value(schema).unwrap_or_default(),
        }
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        Tool::execution_policy(self)
    }

    async fn call_content(&self, args: Value) -> Result<Vec<InputContent>, String> {
        let input = serde_json::from_value::<T::Input>(args).map_err(|e| e.to_string())?;
        let output = self.call(input).await?;
        output.into_tool_result_content()
    }
}
