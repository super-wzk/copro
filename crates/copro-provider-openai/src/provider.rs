use std::sync::Arc;

use crate::config::{OpenAiResponsesModelConfig, OpenAiResponsesProviderConfig};
use crate::error::map_openai_error;
use crate::request::build_response_body;
use crate::stream::OpenAiEventMapper;
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use copro_api::error::{Error, Result};
use copro_api::request::GenerateRequest;
use copro_api::stream::{Model, ModelStream};
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

    pub fn model(
        &self,
        id: impl Into<String>,
        model_config: OpenAiResponsesModelConfig,
    ) -> Result<Arc<dyn Model>> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(Error::client("OpenAI model id cannot be empty"));
        }

        Ok(Arc::new(OpenAiResponsesModel {
            client: self.client.clone(),
            model_config,
            model_id: id,
        }))
    }
}

#[derive(Clone)]
pub struct OpenAiResponsesModel {
    client: Client<OpenAIConfig>,
    model_id: String,
    model_config: OpenAiResponsesModelConfig,
}

impl Model for OpenAiResponsesModel {
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
) -> Result<async_openai::types::stream::StreamResponse<Value>> {
    let responses = client.responses();
    responses
        .create_stream_byot::<_, Value>(body)
        .await
        .map_err(map_openai_error)
}

async fn next_openai_event(
    stream: &mut async_openai::types::stream::StreamResponse<Value>,
) -> Result<Option<Value>> {
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
