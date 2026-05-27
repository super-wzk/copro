use crate::error::{Error, Result};
use crate::message::Message;
use crate::tool::{HostedToolSpec, ToolChoice, ToolDefinition};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GenerateRequestOptions {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl GenerateRequestOptions {
    pub fn extra<T>(&self) -> Result<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_value(Value::Object(self.extra.clone())).map_err(|error| {
            Error::client(format!("invalid generate request options extra: {error}"))
        })
    }

    pub fn insert_extra<T>(&mut self, extra: T) -> Result<()>
    where
        T: Serialize,
    {
        let value = serde_json::to_value(extra).map_err(|error| {
            Error::client(format!(
                "failed to serialize generate request options extra: {error}"
            ))
        })?;
        let Value::Object(extra) = value else {
            return Err(Error::client(
                "generate request options extra must serialize to a JSON object",
            ));
        };

        self.extra.extend(extra);
        Ok(())
    }

    pub fn with_extra<T>(mut self, extra: T) -> Result<Self>
    where
        T: Serialize,
    {
        self.insert_extra(extra)?;
        Ok(self)
    }

    pub fn remove_extra(&mut self, key: &str) -> Option<Value> {
        self.extra.remove(key)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateRequest {
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosted_tools: Vec<HostedToolSpec>,
    pub options: GenerateRequestOptions,
}
