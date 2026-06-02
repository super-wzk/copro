use coox_provider_openai::{
    OpenAiResponsesModelConfig, OpenAiResponsesProvider, OpenAiResponsesProviderConfig,
};
use copro_api::error::Result;
use copro_api::stream::Model;
use std::env;
use std::sync::Arc;

pub const DEFAULT_MODEL: &str = "gpt-5.5";

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeConfig {
    pub provider: OpenAiResponsesProviderConfig,
    pub model: OpenAiResponsesModelConfig,
    pub model_id: String,
}

impl RuntimeConfig {
    pub fn from_env() -> Self {
        Self {
            provider: OpenAiResponsesProviderConfig {
                api_key: env_var("OPENAI_API_KEY"),
                api_base: env_var("OPENAI_API_BASE"),
                organization: env_var("OPENAI_ORGANIZATION"),
                project: env_var("OPENAI_PROJECT"),
            },
            model: OpenAiResponsesModelConfig {
                parallel_tool_calls: Some(true),
                reasoning_effort: None,
                reasoning_summary: Some("auto".to_string()),
                ..OpenAiResponsesModelConfig::default()
            },
            model_id: DEFAULT_MODEL.to_string(),
        }
    }
}

pub fn build_model(config: &RuntimeConfig) -> Result<Arc<dyn Model>> {
    OpenAiResponsesProvider::new(config.provider.clone())
        .model(config.model_id.clone(), config.model.clone())
}

fn env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_non_empty_model_id() {
        let config = RuntimeConfig::from_env();

        assert_eq!(config.model_id, DEFAULT_MODEL);
        assert_eq!(config.model.parallel_tool_calls, Some(true));
    }
}
