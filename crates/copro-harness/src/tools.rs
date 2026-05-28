use copro_agent::ToolRouter;
use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{ImageContent, InputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_api::tool::ToolDefinition;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;

pub trait ToolOutput: Send {
    fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Json<T>(pub T);

impl<T> Json<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> ToolOutput for Json<T>
where
    T: Serialize + Send,
{
    fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String> {
        json_output_content(self.0)
    }
}

impl ToolOutput for String {
    fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String> {
        Ok(vec![InputContent::Text(self)])
    }
}

impl ToolOutput for &'static str {
    fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String> {
        Ok(vec![InputContent::Text(self.to_string())])
    }
}

impl ToolOutput for InputContent {
    fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String> {
        Ok(vec![self])
    }
}

impl ToolOutput for Vec<InputContent> {
    fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String> {
        Ok(self)
    }
}

impl ToolOutput for ImageContent {
    fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String> {
        Ok(vec![InputContent::Image(self)])
    }
}

impl ToolOutput for () {
    fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String> {
        json_output_content(self)
    }
}

macro_rules! impl_json_tool_output {
    ($($ty:ty),* $(,)?) => {
        $(
            impl ToolOutput for $ty {
                fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String> {
                    json_output_content(self)
                }
            }
        )*
    };
}

impl_json_tool_output!(bool, i8, i16, i32, i64, i128, isize);
impl_json_tool_output!(u8, u16, u32, u64, u128, usize);
impl_json_tool_output!(f32, f64);

impl ToolOutput for Value {
    fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String> {
        Ok(vec![InputContent::Text(self.to_string())])
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    type Input: DeserializeOwned + JsonSchema + Send;
    type Output: ToolOutput + Send;

    fn name(&self) -> &str;
    fn description(&self) -> &str;

    async fn call(&self, input: Self::Input) -> std::result::Result<Self::Output, String>;
}

#[async_trait]
pub trait ErasedTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn call_content(&self, args: Value) -> std::result::Result<Vec<InputContent>, String>;
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

    async fn call_content(&self, args: Value) -> std::result::Result<Vec<InputContent>, String> {
        let input = serde_json::from_value::<T::Input>(args).map_err(|e| e.to_string())?;
        let output = self.call(input).await?;
        output.into_tool_result_content()
    }
}

fn json_output_content<T>(output: T) -> std::result::Result<Vec<InputContent>, String>
where
    T: Serialize,
{
    let value = serde_json::to_value(output).map_err(|e| e.to_string())?;
    let text = serde_json::to_string(&value).unwrap_or_else(|_| format!("{value:?}"));
    Ok(vec![InputContent::Text(text)])
}

/// Tool router backed by in-process [`ErasedTool`] implementations.
#[derive(Default, Clone)]
pub struct LocalToolRouter {
    tools: Vec<Arc<dyn ErasedTool>>,
}

impl LocalToolRouter {
    pub fn new(tools: Vec<Arc<dyn ErasedTool>>) -> Self {
        Self { tools }
    }
}

#[async_trait]
impl ToolRouter for LocalToolRouter {
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

/// Tool router that exposes and delegates to multiple child routers.
#[derive(Default, Clone)]
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
