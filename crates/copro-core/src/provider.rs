use crate::error::{ModelError, ModelResult};
use crate::model::{ChatModel, ModelFuture, ModelInfo};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

pub trait ModelProvider: Send + Sync {
    type Config: DeserializeOwned + JsonSchema + Send + Sync + 'static;

    fn list_models(&self) -> ModelFuture<'_, Vec<ModelInfo>>;

    fn model_config_schema(&self) -> Value {
        let schema = schemars::schema_for!(Self::Config);
        serde_json::to_value(schema).unwrap_or_default()
    }

    fn chat_model(&self, model_id: &str, config: Self::Config) -> ModelResult<Arc<dyn ChatModel>>;
}

pub trait ErasedModelProvider: Send + Sync {
    fn list_models(&self) -> ModelFuture<'_, Vec<ModelInfo>>;

    fn model_config_schema(&self) -> Value;

    fn chat_model_json(&self, model_id: &str, config: Value) -> ModelResult<Arc<dyn ChatModel>>;
}

impl<P> ErasedModelProvider for P
where
    P: ModelProvider,
{
    fn list_models(&self) -> ModelFuture<'_, Vec<ModelInfo>> {
        ModelProvider::list_models(self)
    }

    fn model_config_schema(&self) -> Value {
        ModelProvider::model_config_schema(self)
    }

    fn chat_model_json(&self, model_id: &str, config: Value) -> ModelResult<Arc<dyn ChatModel>> {
        let config = serde_json::from_value::<P::Config>(config).map_err(|error| {
            ModelError::client(format!("invalid provider model config: {error}"))
        })?;

        self.chat_model(model_id, config)
    }
}

pub trait ProviderFactory: Send + Sync {
    type Config: DeserializeOwned + JsonSchema + Send + Sync + 'static;

    fn kind(&self) -> &str;

    fn provider_config_schema(&self) -> Value {
        let schema = schemars::schema_for!(Self::Config);
        serde_json::to_value(schema).unwrap_or_default()
    }

    fn build_provider(&self, config: Self::Config) -> ModelResult<Arc<dyn ErasedModelProvider>>;
}

pub trait ErasedProviderFactory: Send + Sync {
    fn kind(&self) -> &str;

    fn provider_config_schema(&self) -> Value;

    fn build_provider_json(&self, config: Value) -> ModelResult<Arc<dyn ErasedModelProvider>>;
}

impl<F> ErasedProviderFactory for F
where
    F: ProviderFactory,
{
    fn kind(&self) -> &str {
        ProviderFactory::kind(self)
    }

    fn provider_config_schema(&self) -> Value {
        ProviderFactory::provider_config_schema(self)
    }

    fn build_provider_json(&self, config: Value) -> ModelResult<Arc<dyn ErasedModelProvider>> {
        let config = serde_json::from_value::<F::Config>(config)
            .map_err(|error| ModelError::client(format!("invalid provider config: {error}")))?;

        self.build_provider(config)
    }
}

#[derive(Default)]
pub struct ProviderRegistry {
    factories: BTreeMap<String, Arc<dyn ErasedProviderFactory>>,
    providers: BTreeMap<String, Arc<dyn ErasedModelProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_factory<F>(&mut self, factory: F) -> Option<Arc<dyn ErasedProviderFactory>>
    where
        F: ErasedProviderFactory + 'static,
    {
        self.register_factory_erased(Arc::new(factory))
    }

    pub fn register_factory_erased(
        &mut self,
        factory: Arc<dyn ErasedProviderFactory>,
    ) -> Option<Arc<dyn ErasedProviderFactory>> {
        let kind = factory.kind().to_string();
        self.factories.insert(kind, factory)
    }

    pub fn factory(&self, kind: &str) -> Option<&dyn ErasedProviderFactory> {
        self.factories.get(kind).map(|factory| factory.as_ref())
    }

    pub fn factories(&self) -> impl Iterator<Item = (&str, &dyn ErasedProviderFactory)> {
        self.factories
            .iter()
            .map(|(kind, factory)| (kind.as_str(), factory.as_ref()))
    }

    pub fn upsert_provider_json(
        &mut self,
        provider_id: &str,
        factory_kind: &str,
        config: Value,
    ) -> ModelResult<Option<Arc<dyn ErasedModelProvider>>> {
        let factory =
            self.factory(factory_kind)
                .ok_or_else(|| ModelError::ProviderFactoryNotFound {
                    factory_kind: factory_kind.to_string(),
                })?;
        let provider = factory.build_provider_json(config)?;

        Ok(self.register_provider_erased(provider_id, provider))
    }

    pub fn register_provider<P>(
        &mut self,
        provider_id: &str,
        provider: P,
    ) -> Option<Arc<dyn ErasedModelProvider>>
    where
        P: ErasedModelProvider + 'static,
    {
        self.register_provider_erased(provider_id, Arc::new(provider))
    }

    pub fn register_provider_erased(
        &mut self,
        provider_id: &str,
        provider: Arc<dyn ErasedModelProvider>,
    ) -> Option<Arc<dyn ErasedModelProvider>> {
        self.providers.insert(provider_id.to_string(), provider)
    }

    pub fn remove_provider(&mut self, provider_id: &str) -> Option<Arc<dyn ErasedModelProvider>> {
        self.providers.remove(provider_id)
    }

    pub fn provider(&self, provider_id: &str) -> Option<&dyn ErasedModelProvider> {
        self.providers
            .get(provider_id)
            .map(|provider| provider.as_ref())
    }

    pub fn providers(&self) -> impl Iterator<Item = (&str, &dyn ErasedModelProvider)> {
        self.providers
            .iter()
            .map(|(provider_id, provider)| (provider_id.as_str(), provider.as_ref()))
    }

    pub fn list_models(&self, provider_id: &str) -> ModelFuture<'_, Vec<ModelInfo>> {
        match self.provider(provider_id) {
            Some(provider) => provider.list_models(),
            None => {
                let provider_id = provider_id.to_string();
                Box::pin(async move { Err(ModelError::ProviderNotFound { provider_id }) })
            }
        }
    }

    pub fn chat_model_json(
        &self,
        provider_id: &str,
        model_id: &str,
        config: Value,
    ) -> ModelResult<Arc<dyn ChatModel>> {
        let provider = self
            .provider(provider_id)
            .ok_or_else(|| ModelError::ProviderNotFound {
                provider_id: provider_id.to_string(),
            })?;

        provider.chat_model_json(model_id, config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InputModality, ModelCapabilities, ModelFeature, ModelInfo};
    use crate::request::GenerateRequest;
    use crate::stream::ModelStream;
    use futures_util::FutureExt;
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct TestProviderConfig {
        expected_model_suffix: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct TestModelConfig {
        suffix: String,
    }

    struct TestProviderFactory;

    impl ProviderFactory for TestProviderFactory {
        type Config = TestProviderConfig;

        fn kind(&self) -> &str {
            "test"
        }

        fn build_provider(
            &self,
            config: Self::Config,
        ) -> ModelResult<Arc<dyn ErasedModelProvider>> {
            Ok(Arc::new(TestProvider {
                expected_model_suffix: config.expected_model_suffix,
            }))
        }
    }

    struct TestProvider {
        expected_model_suffix: String,
    }

    impl ModelProvider for TestProvider {
        type Config = TestModelConfig;

        fn list_models(&self) -> ModelFuture<'_, Vec<ModelInfo>> {
            Box::pin(async { Ok(vec![test_model_info("test-model")]) })
        }

        fn chat_model(
            &self,
            _model_id: &str,
            config: Self::Config,
        ) -> ModelResult<Arc<dyn ChatModel>> {
            if config.suffix != self.expected_model_suffix {
                return Err(ModelError::protocol(format!(
                    "expected model suffix {}, got {}",
                    self.expected_model_suffix, config.suffix
                )));
            }

            Ok(Arc::new(TestModel))
        }
    }

    struct TestModel;

    impl ChatModel for TestModel {
        fn stream(&self, _request: GenerateRequest) -> ModelStream<'_> {
            Box::pin(futures_util::stream::empty())
        }
    }

    fn test_provider() -> TestProvider {
        TestProvider {
            expected_model_suffix: "configured".to_string(),
        }
    }

    fn test_model_info(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            display_name: None,
            capabilities: ModelCapabilities::default()
                .with_input_modality(InputModality::Text)
                .with_feature(ModelFeature::NativeStreaming),
        }
    }

    #[test]
    fn erased_provider_parses_typed_config() {
        let provider: Box<dyn ErasedModelProvider> = Box::new(test_provider());
        provider
            .chat_model_json("model", serde_json::json!({ "suffix": "configured" }))
            .unwrap();
    }

    #[test]
    fn erased_provider_exposes_config_schema() {
        let provider: Box<dyn ErasedModelProvider> = Box::new(test_provider());
        let schema = provider.model_config_schema();

        assert_eq!(schema["title"], "TestModelConfig");
    }

    #[test]
    fn erased_factory_parses_provider_config() {
        let factory: Box<dyn ErasedProviderFactory> = Box::new(TestProviderFactory);
        let provider = factory
            .build_provider_json(serde_json::json!({ "expected_model_suffix": "configured" }))
            .unwrap();

        provider
            .chat_model_json("model", serde_json::json!({ "suffix": "configured" }))
            .unwrap();
    }

    #[test]
    fn registry_routes_model_construction_by_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register_provider("test", test_provider());

        registry
            .chat_model_json(
                "test",
                "model",
                serde_json::json!({ "suffix": "configured" }),
            )
            .unwrap();
    }

    #[test]
    fn registry_lists_provider_models() {
        let mut registry = ProviderRegistry::new();
        registry.register_provider("test", test_provider());

        let models = registry
            .list_models("test")
            .now_or_never()
            .unwrap()
            .unwrap();

        assert_eq!(models[0].id, "test-model");
    }

    #[test]
    fn registry_reports_missing_provider() {
        let registry = ProviderRegistry::new();
        let Err(error) = registry.chat_model_json("missing", "model", serde_json::json!({})) else {
            panic!("expected missing provider error");
        };

        assert_eq!(
            error,
            ModelError::ProviderNotFound {
                provider_id: "missing".to_string(),
            }
        );
    }

    #[test]
    fn registry_reports_missing_factory() {
        let mut registry = ProviderRegistry::new();
        let Err(error) = registry.upsert_provider_json("local", "missing", serde_json::json!({}))
        else {
            panic!("expected missing factory error");
        };

        assert_eq!(
            error,
            ModelError::ProviderFactoryNotFound {
                factory_kind: "missing".to_string(),
            }
        );
    }

    #[test]
    fn registry_replaces_provider_from_factory_config() {
        let mut registry = ProviderRegistry::new();
        registry.register_factory(TestProviderFactory);

        registry
            .upsert_provider_json(
                "local",
                "test",
                serde_json::json!({ "expected_model_suffix": "first" }),
            )
            .unwrap();
        registry
            .chat_model_json("local", "model", serde_json::json!({ "suffix": "first" }))
            .unwrap();

        registry
            .upsert_provider_json(
                "local",
                "test",
                serde_json::json!({ "expected_model_suffix": "second" }),
            )
            .unwrap();

        assert!(
            registry
                .chat_model_json("local", "model", serde_json::json!({ "suffix": "first" }))
                .is_err()
        );
        registry
            .chat_model_json("local", "model", serde_json::json!({ "suffix": "second" }))
            .unwrap();
    }
}
