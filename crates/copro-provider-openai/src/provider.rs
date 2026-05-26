use std::sync::Arc;

use crate::config::{OpenAiResponsesModelConfig, OpenAiResponsesProviderConfig};
use crate::error::map_openai_error;
use crate::request::build_response_body;
use crate::stream::OpenAiEventMapper;
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::responses::ResponseStreamEvent;
use copro_core::error::{Error, Result};
use copro_core::model::{InputModality, ModelCapabilities, ModelDefinition, ModelFeature};
use copro_core::provider::{Chat, Provider};
use copro_core::request::GenerateRequest;
use copro_core::stream::ModelStream;
use futures_util::StreamExt;
use serde_json::Value;

#[derive(Clone)]
pub struct OpenAiResponsesProvider {
    client: Client<OpenAIConfig>,
}

impl OpenAiResponsesProvider {
    pub fn new(config: OpenAiResponsesProviderConfig) -> Self {
        Self {
            client: Client::with_config(openai_config(config)),
        }
    }

    /// Build a [`ModelDefinition`] bound to this provider.
    pub fn model_definition(
        &self,
        id: impl Into<String>,
        config: OpenAiResponsesModelConfig,
    ) -> Result<ModelDefinition> {
        ModelDefinition::new(self.id(), id).with_config(config)
    }
}

/// Pre-built [`ModelDefinition`] for `gpt-5.5` with default configuration.
pub fn gpt_5_5() -> Result<ModelDefinition> {
    ModelDefinition::new("openai-responses", "gpt-5.5")
        .with_capabilities(
            ModelCapabilities::default()
                .with_input_modality(InputModality::Text)
                .with_input_modality(InputModality::Image)
                .with_feature(ModelFeature::NativeStreaming)
                .with_feature(ModelFeature::Tools)
                .with_feature(ModelFeature::ToolChoice)
                .with_feature(ModelFeature::Thinking),
        )
        .with_config(OpenAiResponsesModelConfig::default())
}

/// Pre-built [`ModelDefinition`] for `gpt-5.4` with default configuration.
pub fn gpt_5_4() -> Result<ModelDefinition> {
    ModelDefinition::new("openai-responses", "gpt-5.4")
        .with_capabilities(
            ModelCapabilities::default()
                .with_input_modality(InputModality::Text)
                .with_input_modality(InputModality::Image)
                .with_feature(ModelFeature::NativeStreaming)
                .with_feature(ModelFeature::Tools)
                .with_feature(ModelFeature::ToolChoice)
                .with_feature(ModelFeature::Thinking),
        )
        .with_config(OpenAiResponsesModelConfig::default())
}

impl Provider for OpenAiResponsesProvider {
    fn id(&self) -> &str {
        "openai-responses"
    }

    fn chat(&self, id: &str, config: Value) -> Result<Arc<dyn Chat>> {
        let model_config: OpenAiResponsesModelConfig = serde_json::from_value(config)
            .map_err(|e| Error::client(format!("invalid model config: {e}")))?;

        if id.trim().is_empty() {
            return Err(Error::client("OpenAI model id cannot be empty"));
        }

        Ok(Arc::new(OpenAiResponsesChat {
            client: self.client.clone(),
            model_config,
            model_id: id.to_string(),
        }))
    }
}

#[derive(Clone)]
pub struct OpenAiResponsesChat {
    client: Client<OpenAIConfig>,
    model_id: String,
    model_config: OpenAiResponsesModelConfig,
}

impl Chat for OpenAiResponsesChat {
    fn stream(&self, request: GenerateRequest) -> ModelStream<'_> {
        let body = match build_response_body(&self.model_id, &self.model_config, request) {
            Ok(body) => body,
            Err(error) => return Box::pin(futures_util::stream::once(async move { Err(error) })),
        };
        let client = self.client.clone();

        Box::pin(async_stream::try_stream! {
            let mut stream = create_response_stream(&client, body).await?;
            let mut mapper = OpenAiEventMapper::new();

            loop {
                let next = next_openai_event(&mut stream).await?;
                let Some(event) = next else {
                    break;
                };

                for output in mapper.map_event(event)? {
                    yield output;
                }
            }
        })
    }
}

async fn create_response_stream(
    client: &Client<OpenAIConfig>,
    body: Value,
) -> Result<async_openai::types::responses::ResponseStream> {
    let responses = client.responses();
    responses
        .create_stream_byot::<_, ResponseStreamEvent>(body)
        .await
        .map_err(map_openai_error)
}

async fn next_openai_event(
    stream: &mut async_openai::types::responses::ResponseStream,
) -> Result<Option<ResponseStreamEvent>> {
    stream.next().await.transpose().map_err(map_openai_error)
}

fn openai_config(config: OpenAiResponsesProviderConfig) -> OpenAIConfig {
    let mut openai_config = OpenAIConfig::new();

    if let Some(api_key) = non_empty(config.api_key) {
        openai_config = openai_config.with_api_key(api_key);
    }
    if let Some(api_base) = non_empty(config.api_base) {
        openai_config = openai_config.with_api_base(api_base);
    }
    if let Some(organization) = non_empty(config.organization) {
        openai_config = openai_config.with_org_id(organization);
    }
    if let Some(project) = non_empty(config.project) {
        openai_config = openai_config.with_project_id(project);
    }

    openai_config
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}
