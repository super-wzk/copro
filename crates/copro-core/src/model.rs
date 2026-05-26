use crate::error::{ModelError, ModelResult};
use crate::request::GenerateRequest;
use crate::response::GenerateResponse;
use crate::stream::{ModelStream, OutputStreamState};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

pub type ModelFuture<'a, T> = Pin<Box<dyn Future<Output = ModelResult<T>> + Send + 'a>>;

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

    pub fn extra<T>(&self) -> ModelResult<T>
    where
        T: DeserializeOwned,
    {
        serde_json::from_value(Value::Object(self.extra.clone())).map_err(|error| {
            ModelError::client(format!("invalid model capabilities extra: {error}"))
        })
    }

    pub fn insert_extra<T>(&mut self, extra: T) -> ModelResult<()>
    where
        T: Serialize,
    {
        let value = serde_json::to_value(extra).map_err(|error| {
            ModelError::client(format!(
                "failed to serialize model capabilities extra: {error}"
            ))
        })?;
        let Value::Object(extra) = value else {
            return Err(ModelError::client(
                "model capabilities extra must serialize to a JSON object",
            ));
        };

        self.extra.extend(extra);
        Ok(())
    }

    pub fn with_extra<T>(mut self, extra: T) -> ModelResult<Self>
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
    pub display_name: Option<String>,
    pub capabilities: ModelCapabilities,
}

pub trait ChatModel: Send + Sync {
    /// Starts a streaming generation request.
    ///
    /// Implementations should return promptly and defer network I/O until the
    /// returned stream is polled, so runtimes can apply deadlines and cancellation.
    fn stream(&self, request: GenerateRequest) -> ModelStream<'_>;

    fn generate(&self, request: GenerateRequest) -> ModelFuture<'_, GenerateResponse> {
        Box::pin(async move { OutputStreamState::collect(self.stream(request)).await })
    }
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

        assert!(matches!(error, ModelError::Client { .. }));
    }

    #[test]
    fn non_object_extra_is_rejected() {
        let mut capabilities = ModelCapabilities::default();

        let error = capabilities.insert_extra(42).unwrap_err();

        assert!(matches!(error, ModelError::Client { .. }));
    }
}
