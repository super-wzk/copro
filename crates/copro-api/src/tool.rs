use crate::async_trait;
use crate::error::{Error, Result};
use crate::message::{ImageContent, InputContent};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// tool parameters schema
    pub parameters: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostedToolSpec {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub parameters: Map<String, Value>,
}

impl HostedToolSpec {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            parameters: Map::new(),
        }
    }

    pub fn parameters<T>(&self) -> Result<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_value(Value::Object(self.parameters.clone())).map_err(|error| {
            Error::client(format!(
                "invalid hosted tool `{}` parameters: {error}",
                self.kind
            ))
        })
    }

    pub fn insert_parameters<T>(&mut self, parameters: T) -> Result<()>
    where
        T: Serialize,
    {
        let value = serde_json::to_value(parameters).map_err(|error| {
            Error::client(format!(
                "failed to serialize hosted tool `{}` parameters: {error}",
                self.kind
            ))
        })?;
        let Value::Object(parameters) = value else {
            return Err(Error::client(format!(
                "hosted tool `{}` parameters must serialize to a JSON object",
                self.kind
            )));
        };

        self.parameters.extend(parameters);
        Ok(())
    }

    pub fn with_parameters<T>(mut self, parameters: T) -> Result<Self>
    where
        T: Serialize,
    {
        self.insert_parameters(parameters)?;
        Ok(self)
    }

    pub fn remove_parameter(&mut self, key: &str) -> Option<Value> {
        self.parameters.remove(key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Specific { name: String },
}

/// A typed tool return value that knows how to become model-visible tool output.
///
/// This makes rich output explicit at the associated type level. A tool returning
/// an image can use `type Output = ImageContent`; a tool returning mixed content
/// can use `type Output = Vec<InputContent>`; structured JSON can be wrapped in
/// [`Json<T>`].
pub trait ToolOutput: Send {
    fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String>;
}

/// Explicit wrapper for tool outputs that should be sent as JSON text.
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

    /// Execute the tool and return provider-independent result content.
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

impl From<&dyn ErasedTool> for ToolDefinition {
    fn from(tool: &dyn ErasedTool) -> Self {
        tool.definition()
    }
}

#[cfg(test)]
mod tests {
    use crate::async_trait;
    use crate::message::{ImageContent, InputContent};
    use crate::tool::{ErasedTool, Json, Tool};
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, JsonSchema)]
    struct HelloArgs {
        text: String,
    }
    struct Hello;
    #[async_trait]
    impl Tool for Hello {
        type Input = HelloArgs;
        type Output = String;
        fn name(&self) -> &str {
            "hello"
        }
        fn description(&self) -> &str {
            "Hello"
        }
        async fn call(&self, input: Self::Input) -> Result<Self::Output, String> {
            Ok(format!("Hello {}!", input.text))
        }
    }

    #[derive(Serialize)]
    struct HelloJson {
        text: String,
    }

    struct HelloJsonTool;
    #[async_trait]
    impl Tool for HelloJsonTool {
        type Input = HelloArgs;
        type Output = Json<HelloJson>;
        fn name(&self) -> &str {
            "hello_json"
        }
        fn description(&self) -> &str {
            "Hello JSON"
        }
        async fn call(&self, input: Self::Input) -> Result<Self::Output, String> {
            Ok(Json::new(HelloJson { text: input.text }))
        }
    }

    struct Screenshot;
    #[async_trait]
    impl Tool for Screenshot {
        type Input = HelloArgs;
        type Output = ImageContent;
        fn name(&self) -> &str {
            "screenshot"
        }
        fn description(&self) -> &str {
            "Return a screenshot"
        }
        async fn call(&self, input: Self::Input) -> Result<Self::Output, String> {
            Ok(ImageContent::Url { url: input.text })
        }
    }

    #[tokio::test]
    async fn erased_tool_string_content_is_plain_text() {
        let tools: Box<dyn ErasedTool> = Box::new(Hello);
        let res = tools
            .call_content(serde_json::json!({"text":"World"}))
            .await
            .unwrap();
        assert_eq!(res, vec![InputContent::Text("Hello World!".to_string())]);
    }

    #[tokio::test]
    async fn erased_tool_json_wrapper_outputs_json_text() {
        let tools: Box<dyn ErasedTool> = Box::new(HelloJsonTool);
        let content = tools
            .call_content(serde_json::json!({"text":"World"}))
            .await
            .unwrap();

        assert_eq!(
            content,
            vec![InputContent::Text("{\"text\":\"World\"}".to_string())]
        );
    }

    #[tokio::test]
    async fn erased_tool_content_can_return_images() {
        let tools: Box<dyn ErasedTool> = Box::new(Screenshot);
        let res = tools
            .call_content(serde_json::json!({"text":"https://example.com/a.png"}))
            .await
            .unwrap();
        assert_eq!(
            res,
            vec![InputContent::Image(ImageContent::Url {
                url: "https://example.com/a.png".to_string(),
            })]
        );
    }
}
