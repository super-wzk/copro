use std::sync::Arc;

use crate::capabilities::infer_capabilities;
use crate::config::{OpenAiResponsesModelConfig, OpenAiResponsesProviderConfig};
use crate::error::map_openai_error;
use crate::request::build_response_body;
use crate::stream::OpenAiEventMapper;
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::responses::ResponseStreamEvent;
use copro_core::error::{ModelError, ModelResult};
use copro_core::model::{ChatModel, ModelFuture, ModelInfo};
use copro_core::provider::ModelProvider;
use copro_core::request::GenerateRequest;
use copro_core::stream::ModelStream;
use copro_derive::CoproProviderFactory;
use futures_util::StreamExt;
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, CoproProviderFactory)]
#[provider(
    kind = "openai-responses",
    config = OpenAiResponsesProviderConfig,
    provider = OpenAiResponsesProvider
)]
pub struct OpenAiResponsesProviderFactory;

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
}

impl ModelProvider for OpenAiResponsesProvider {
    type Config = OpenAiResponsesModelConfig;

    fn list_models(&self) -> ModelFuture<'_, Vec<ModelInfo>> {
        let client = self.client.clone();

        Box::pin(async move {
            let response = client.models().list().await.map_err(map_openai_error)?;
            let mut models = response
                .data
                .into_iter()
                .map(|model| ModelInfo {
                    capabilities: infer_capabilities(&model.id),
                    display_name: None,
                    id: model.id,
                })
                .collect::<Vec<_>>();
            models.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(models)
        })
    }

    fn chat_model(&self, model_id: &str, config: Self::Config) -> ModelResult<Arc<dyn ChatModel>> {
        if model_id.trim().is_empty() {
            return Err(ModelError::client("OpenAI model id cannot be empty"));
        }

        Ok(Arc::new(OpenAiResponsesChatModel {
            client: self.client.clone(),
            model_config: config,
            model_id: model_id.to_string(),
        }))
    }
}

#[derive(Clone)]
pub struct OpenAiResponsesChatModel {
    client: Client<OpenAIConfig>,
    model_id: String,
    model_config: OpenAiResponsesModelConfig,
}

impl ChatModel for OpenAiResponsesChatModel {
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
) -> ModelResult<async_openai::types::responses::ResponseStream> {
    let responses = client.responses();
    responses
        .create_stream_byot::<_, ResponseStreamEvent>(body)
        .await
        .map_err(map_openai_error)
}

async fn next_openai_event(
    stream: &mut async_openai::types::responses::ResponseStream,
) -> ModelResult<Option<ResponseStreamEvent>> {
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
