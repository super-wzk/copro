use crate::error::{Error, Result};
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
