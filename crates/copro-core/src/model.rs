use crate::error::{Error, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

pub type ModelFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelFeature {
    NativeStreaming,
    Tools,
    ToolChoice,
    Thinking,
    JsonSchema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputModality {
    Text,
    Image,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub input_modalities: BTreeSet<InputModality>,
    #[serde(default)]
    pub features: BTreeSet<ModelFeature>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl ModelCapabilities {
    pub fn supports(&self, feature: ModelFeature) -> bool {
        self.features.contains(&feature)
    }

    pub fn accepts(&self, modality: InputModality) -> bool {
        self.input_modalities.contains(&modality)
    }

    pub fn with_feature(mut self, feature: ModelFeature) -> Self {
        self.features.insert(feature);
        self
    }

    pub fn with_input_modality(mut self, modality: InputModality) -> Self {
        self.input_modalities.insert(modality);
        self
    }

    pub fn extra<T>(&self) -> Result<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_value(Value::Object(self.extra.clone()))
            .map_err(|error| Error::client(format!("invalid model capabilities extra: {error}")))
    }

    pub fn insert_extra<T>(&mut self, extra: T) -> Result<()>
    where
        T: Serialize,
    {
        let value = serde_json::to_value(extra).map_err(|error| {
            Error::client(format!(
                "failed to serialize model capabilities extra: {error}"
            ))
        })?;
        let Value::Object(extra) = value else {
            return Err(Error::client(
                "model capabilities extra must serialize to a JSON object",
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: Option<String>,
    pub capabilities: ModelCapabilities,
}

/// A locally configured model entry bound to a provider instance.
///
/// `provider_id` points at a registered provider in `ProviderRegistry`., while
/// `id` is the model identifier used both by the registry and by that provider
/// (for example `gpt-5.5` for OpenAI). This keeps model discovery and model
/// configuration out of provider implementations: providers only know how to
/// construct a chat model for an explicit id and provider-specific config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDefinition {
    pub provider_id: String,
    pub id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    #[serde(default = "default_model_config")]
    pub config: Value,
}

impl ModelDefinition {
    pub fn new(provider_id: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            id: id.into(),
            name: None,
            capabilities: ModelCapabilities::default(),
            config: default_model_config(),
        }
    }

    pub fn info(&self) -> ModelInfo {
        ModelInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            capabilities: self.capabilities.clone(),
        }
    }

    pub fn config<T>(&self) -> Result<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_value(self.config.clone())
            .map_err(|error| Error::client(format!("invalid model config: {error}")))
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_capabilities(mut self, capabilities: ModelCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn with_config<T>(mut self, config: T) -> Result<Self>
    where
        T: Serialize,
    {
        self.config = serde_json::to_value(config)
            .map_err(|error| Error::client(format!("failed to serialize model config: {error}")))?;
        Ok(self)
    }
}

fn default_model_config() -> Value {
    Value::Object(Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct TestExtra {
        value: Option<String>,
    }

    #[test]
    fn typed_extra_round_trips() {
        let mut capabilities = ModelCapabilities::default();
        capabilities
            .insert_extra(TestExtra {
                value: Some("configured".to_string()),
            })
            .unwrap();

        let extra = capabilities.extra::<TestExtra>().unwrap();

        assert_eq!(
            extra,
            TestExtra {
                value: Some("configured".to_string()),
            }
        );
    }

    #[test]
    fn empty_extra_deserializes_to_default() {
        let capabilities = ModelCapabilities::default();

        let extra = capabilities.extra::<TestExtra>().unwrap();

        assert_eq!(extra, TestExtra::default());
    }

    #[test]
    fn invalid_extra_reports_client_error() {
        let mut capabilities = ModelCapabilities::default();
        capabilities
            .extra
            .insert("value".to_string(), serde_json::json!(42));

        let error = capabilities.extra::<TestExtra>().unwrap_err();

        assert!(matches!(error, Error::Client { .. }));
    }

    #[test]
    fn non_object_extra_is_rejected() {
        let mut capabilities = ModelCapabilities::default();

        let error = capabilities.insert_extra(42).unwrap_err();

        assert!(matches!(error, Error::Client { .. }));
    }
}
