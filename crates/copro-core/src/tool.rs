use crate::error::{Error, Result};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
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

pub trait Tool {
    type Input: DeserializeOwned + JsonSchema;
    type Output: Serialize;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn call(&self, input: Self::Input) -> std::result::Result<Self::Output, String>;
}

pub trait ErasedTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn call_json(&self, args: Value) -> std::result::Result<Value, String>;
}

impl<T: Tool + Send + Sync> ErasedTool for T {
    fn name(&self) -> &str {
        Tool::name(self)
    }
    fn description(&self) -> &str {
        Tool::description(self)
    }
    fn parameters(&self) -> Value {
        let schema = schemars::schema_for!(T::Input);
        serde_json::to_value(schema).unwrap_or_default()
    }
    fn call_json(&self, args: Value) -> std::result::Result<Value, String> {
        let input = serde_json::from_value::<T::Input>(args).map_err(|e| e.to_string())?;
        let output = self.call(input)?;
        serde_json::to_value(output).map_err(|e| e.to_string())
    }
}

impl From<&dyn ErasedTool> for ToolDefinition {
    fn from(tool: &dyn ErasedTool) -> Self {
        Self {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            parameters: tool.parameters(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::error::Error;
    use crate::tool::{ErasedTool, HostedToolSpec, Tool};
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, JsonSchema)]
    struct HelloArgs {
        text: String,
    }
    struct Hello;
    impl Tool for Hello {
        type Input = HelloArgs;
        type Output = String;
        fn name(&self) -> &str {
            "hello"
        }
        fn description(&self) -> &str {
            "Hello"
        }
        fn call(&self, input: Self::Input) -> Result<Self::Output, String> {
            Ok(format!("Hello {}!", input.text))
        }
    }

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct TestHostedParameters {
        partial_images: Option<u8>,
    }

    #[test]
    fn hosted_tool_parameters_round_trip() {
        let tool = HostedToolSpec::new("image_generation")
            .with_parameters(TestHostedParameters {
                partial_images: Some(2),
            })
            .unwrap();

        assert_eq!(tool.kind, "image_generation");
        assert_eq!(
            tool.parameters::<TestHostedParameters>().unwrap(),
            TestHostedParameters {
                partial_images: Some(2),
            }
        );
    }

    #[test]
    fn hosted_tool_rejects_non_object_parameters() {
        let mut tool = HostedToolSpec::new("image_generation");

        let error = tool.insert_parameters(42).unwrap_err();

        assert!(matches!(error, Error::Client { .. }));
    }

    #[test]
    fn test_erased_tool() {
        let tools: Box<dyn ErasedTool> = Box::new(Hello);
        let res = tools
            .call_json(serde_json::json!({"text":"World"}))
            .unwrap();
        assert_eq!(res, "Hello World!");
    }
}
