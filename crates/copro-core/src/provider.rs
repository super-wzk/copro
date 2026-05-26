use crate::error::{Error, Result};
use crate::model::{ModelDefinition, ModelFuture, ModelInfo};
use crate::request::GenerateRequest;
use crate::response::GenerateResponse;
use crate::stream::{ModelStream, OutputStreamState};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// A live chat session bound to a specific model through a provider.
pub trait Chat: Send + Sync {
    /// Starts a streaming generation request.
    fn stream(&self, request: GenerateRequest) -> ModelStream<'_>;

    fn generate(&self, request: GenerateRequest) -> ModelFuture<'_, GenerateResponse> {
        Box::pin(async move { OutputStreamState::collect(self.stream(request)).await })
    }
}

/// An API backend that constructs [`Chat`] instances for upstream model ids.
pub trait Provider: Send + Sync {
    /// Globally unique provider identifier (e.g. `"openai-responses"`).
    fn id(&self) -> &str;

    fn chat(&self, id: &str, config: Value) -> Result<Arc<dyn Chat>>;
}

/// Central registry that owns providers (API backends) and models (capability
/// descriptors bound to a provider).
#[derive(Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn Provider>>,
    models: BTreeMap<String, ModelDefinition>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- providers ----------------------------------------------------------

    pub fn register_provider<P>(&mut self, provider: P) -> Option<Arc<dyn Provider>>
    where
        P: Provider + 'static,
    {
        let id = provider.id().to_string();
        self.providers.insert(id, Arc::new(provider))
    }

    pub fn remove_provider(&mut self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers.remove(id)
    }

    pub fn provider(&self, id: &str) -> Option<&dyn Provider> {
        self.providers.get(id).map(|p| p.as_ref())
    }

    // ---- models -------------------------------------------------------------

    pub fn register_model(&mut self, model: ModelDefinition) -> Option<ModelDefinition> {
        let id = model.id.clone();
        self.models.insert(id, model)
    }

    pub fn remove_model(&mut self, model_id: &str) -> Option<ModelDefinition> {
        self.models.remove(model_id)
    }

    pub fn model(&self, model_id: &str) -> Option<&ModelDefinition> {
        self.models.get(model_id)
    }

    pub fn list_models(&self) -> Vec<ModelInfo> {
        self.models.values().map(|model| model.info()).collect()
    }

    pub fn chat(&self, model_id: &str) -> Result<Arc<dyn Chat>> {
        let model = self.model(model_id).ok_or_else(|| Error::ModelNotFound {
            model_id: model_id.to_string(),
        })?;
        let provider =
            self.provider(&model.provider_id)
                .ok_or_else(|| Error::ProviderNotFound {
                    provider_id: model.provider_id.clone(),
                })?;

        provider.chat(&model.id, model.config.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InputModality, ModelCapabilities, ModelFeature};
    use crate::request::GenerateRequest;
    use crate::stream::ModelStream;
    use serde::{Deserialize, Serialize};

    // ---- test provider ------------------------------------------------------

    #[derive(Debug)]
    struct TestProvider {
        expected_id: String,
        expected_suffix: String,
    }

    impl TestProvider {
        fn new(expected_id: &str, expected_suffix: &str) -> Self {
            Self {
                expected_id: expected_id.to_string(),
                expected_suffix: expected_suffix.to_string(),
            }
        }
    }

    impl Provider for TestProvider {
        fn id(&self) -> &str {
            "test"
        }

        fn chat(&self, id: &str, config: Value) -> Result<Arc<dyn Chat>> {
            let cfg: TestModelConfig = serde_json::from_value(config)
                .map_err(|e| Error::client(format!("invalid model config: {e}")))?;

            if id != self.expected_id {
                return Err(Error::protocol(format!(
                    "expected id {}, got {}",
                    self.expected_id, id
                )));
            }
            if cfg.suffix != self.expected_suffix {
                return Err(Error::protocol(format!(
                    "expected suffix {}, got {}",
                    self.expected_suffix, cfg.suffix
                )));
            }
            Ok(Arc::new(TestChat))
        }
    }

    struct TestChat;

    impl Chat for TestChat {
        fn stream(&self, _request: GenerateRequest) -> ModelStream<'_> {
            Box::pin(futures_util::stream::empty())
        }
    }

    // ---- config types -------------------------------------------------------

    #[derive(Debug, Deserialize, Serialize)]
    struct TestModelConfig {
        suffix: String,
    }

    // ---- helpers ------------------------------------------------------------

    fn test_model(provider_id: &str, id: &str, suffix: &str) -> ModelDefinition {
        ModelDefinition::new(provider_id, id)
            .with_name("Test Model")
            .with_capabilities(
                ModelCapabilities::default()
                    .with_input_modality(InputModality::Text)
                    .with_feature(ModelFeature::NativeStreaming),
            )
            .with_config(TestModelConfig {
                suffix: suffix.to_string(),
            })
            .unwrap()
    }

    // ---- tests --------------------------------------------------------------

    #[test]
    fn provider_chat_passes_id_and_config() {
        let provider = TestProvider::new("gpt-4", "configured");
        provider
            .chat("gpt-4", serde_json::json!({"suffix": "configured"}))
            .unwrap();
    }

    #[test]
    fn registry_routes_chat_through_model_and_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register_provider(TestProvider::new("gpt-4", "cfg"));
        registry.register_model(test_model("test", "gpt-4", "cfg"));

        registry.chat("gpt-4").unwrap();
    }

    #[test]
    fn registry_lists_registered_models() {
        let mut registry = ProviderRegistry::new();
        registry.register_model(test_model("test", "gpt-4", "cfg"));

        let models = registry.list_models();
        assert_eq!(models[0].id, "gpt-4");
        assert_eq!(models[0].name.as_deref(), Some("Test Model"));
    }

    #[test]
    fn registry_reports_missing_model() {
        let registry = ProviderRegistry::new();
        let Err(e) = registry.chat("missing") else {
            panic!("expected error")
        };
        assert_eq!(
            e,
            Error::ModelNotFound {
                model_id: "missing".into()
            }
        );
    }

    #[test]
    fn registry_reports_missing_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register_model(test_model("missing", "gpt-4", "cfg"));

        let Err(e) = registry.chat("gpt-4") else {
            panic!("expected error")
        };
        assert_eq!(
            e,
            Error::ProviderNotFound {
                provider_id: "missing".into()
            }
        );
    }

    #[test]
    fn registry_replace_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register_provider(TestProvider::new("m1", "first"));
        registry.register_model(test_model("test", "m1", "first"));
        registry.chat("m1").unwrap();

        registry.register_provider(TestProvider::new("m1", "second"));
        assert!(registry.chat("m1").is_err());

        registry.register_model(test_model("test", "m1", "second"));
        registry.chat("m1").unwrap();
    }
}
