use copro_core::error::ModelResult;
use copro_core::model::{ChatModel, ModelFuture, ModelInfo};
use copro_core::provider::{ErasedProviderFactory, ModelProvider, ProviderFactory};
use copro_core::request::GenerateRequest;
use copro_core::stream::ModelStream;
use copro_derive::CoproProviderFactory;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize, JsonSchema)]
struct TestProviderConfig {
    token: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TestModelConfig {
    marker: String,
}

#[derive(CoproProviderFactory)]
#[provider(kind = "test", config = TestProviderConfig, provider = TestProvider)]
struct TestProviderFactory;

#[derive(CoproProviderFactory)]
#[provider(
    kind = "custom",
    config = TestProviderConfig,
    provider = TestProvider,
    constructor = build_test_provider
)]
struct CustomConstructorFactory;

struct TestProvider {
    token: String,
}

impl TestProvider {
    fn new(config: TestProviderConfig) -> Self {
        Self {
            token: config.token,
        }
    }
}

fn build_test_provider(config: TestProviderConfig) -> TestProvider {
    TestProvider {
        token: format!("custom-{}", config.token),
    }
}

impl ModelProvider for TestProvider {
    type Config = TestModelConfig;

    fn list_models(&self) -> ModelFuture<'_, Vec<ModelInfo>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn chat_model(&self, _model_id: &str, config: Self::Config) -> ModelResult<Arc<dyn ChatModel>> {
        assert_eq!(config.marker, self.token);
        Ok(Arc::new(TestChatModel))
    }
}

struct TestChatModel;

impl ChatModel for TestChatModel {
    fn stream(&self, _request: GenerateRequest) -> ModelStream<'_> {
        Box::pin(futures_util::stream::empty())
    }
}

#[test]
fn derived_factory_builds_provider_from_json() {
    let factory = TestProviderFactory;

    assert_eq!(ProviderFactory::kind(&factory), "test");

    let provider = ErasedProviderFactory::build_provider_json(
        &factory,
        serde_json::json!({ "token": "configured" }),
    )
    .unwrap();

    provider
        .chat_model_json("model", serde_json::json!({ "marker": "configured" }))
        .unwrap();
}

#[test]
fn derived_factory_supports_custom_constructor() {
    let factory = CustomConstructorFactory;

    assert_eq!(ProviderFactory::kind(&factory), "custom");

    let provider = ErasedProviderFactory::build_provider_json(
        &factory,
        serde_json::json!({ "token": "configured" }),
    )
    .unwrap();

    provider
        .chat_model_json(
            "model",
            serde_json::json!({ "marker": "custom-configured" }),
        )
        .unwrap();
}
