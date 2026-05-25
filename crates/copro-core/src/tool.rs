use crate::types::ToolDefinition;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub trait Tool {
    type Input: DeserializeOwned + JsonSchema;
    type Output: Serialize;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn call(&self, input: Self::Input) -> Result<Self::Output, String>;
}

pub trait ErasedTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn call_json(&self, args: Value) -> Result<Value, String>;
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
    fn call_json(&self, args: Value) -> Result<Value, String> {
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
    use crate::tool::{ErasedTool, Tool};
    use schemars::JsonSchema;
    use serde::Deserialize;

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

    #[test]
    fn test_erased_tool() {
        let tools: Box<dyn ErasedTool> = Box::new(Hello);
        let res = tools
            .call_json(serde_json::json!({"text":"World"}))
            .unwrap();
        assert_eq!(res, "Hello World!");
    }
}
