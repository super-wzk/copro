use crate::error::ModelResult;
use crate::request::GenerateRequest;
use crate::response::GenerateResponse;
use crate::stream::{AssistantStreamState, ModelStream};
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
    #[serde(default)]
    pub extensions: Map<String, Value>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    pub capabilities: ModelCapabilities,
}

pub trait ChatModel: Send + Sync {
    fn stream(&self, request: GenerateRequest) -> ModelStream<'_>;

    fn generate(&self, request: GenerateRequest) -> ModelFuture<'_, GenerateResponse> {
        Box::pin(async move { AssistantStreamState::collect(self.stream(request)).await })
    }
}
