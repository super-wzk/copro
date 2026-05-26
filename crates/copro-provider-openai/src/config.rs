use copro_derive::CoproHostedTool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct OpenAiResponsesProviderConfig {
    pub api_key: Option<String>,
    pub api_base: Option<String>,
    pub organization: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct OpenAiResponsesModelConfig {
    pub store: Option<bool>,
    pub parallel_tool_calls: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub reasoning_summary: Option<String>,
    pub extra_body: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, rename_all = "camelCase")]
pub struct OpenAiResponsesRequestOptions {
    pub extra_body: Map<String, Value>,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, CoproHostedTool,
)]
#[serde(default)]
#[hosted_tool(kind = "image_generation")]
pub struct OpenAiImageGenerationTool {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_images: Option<u8>,
}
