use copro_core::model::{InputModality, ModelCapabilities, ModelFeature};

pub(crate) fn infer_capabilities(model_id: &str) -> ModelCapabilities {
    let mut capabilities = ModelCapabilities::default()
        .with_input_modality(InputModality::Text)
        .with_feature(ModelFeature::NativeStreaming)
        .with_feature(ModelFeature::Tools)
        .with_feature(ModelFeature::ToolChoice);

    if is_multimodal_model(model_id) {
        capabilities = capabilities.with_input_modality(InputModality::Image);
    }
    if is_reasoning_model(model_id) {
        capabilities = capabilities.with_feature(ModelFeature::Thinking);
    }

    capabilities
}

fn is_multimodal_model(model_id: &str) -> bool {
    model_id.starts_with("gpt-5")
}

fn is_reasoning_model(model_id: &str) -> bool {
    model_id.starts_with("gpt-5")
}
