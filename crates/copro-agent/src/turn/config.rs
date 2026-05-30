use copro_api::request::GenerateRequestOptions;
use copro_api::tool::{HostedToolSpec, ToolChoice};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentTurnConfig {
    #[serde(default)]
    options: GenerateRequestOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hosted_tools: Vec<HostedToolSpec>,
}

impl AgentTurnConfig {
    pub fn new(
        options: GenerateRequestOptions,
        tool_choice: Option<ToolChoice>,
        hosted_tools: Vec<HostedToolSpec>,
    ) -> Self {
        Self {
            options,
            tool_choice,
            hosted_tools,
        }
    }

    pub fn options(&self) -> &GenerateRequestOptions {
        &self.options
    }

    pub fn tool_choice(&self) -> Option<&ToolChoice> {
        self.tool_choice.as_ref()
    }

    pub fn hosted_tools(&self) -> &[HostedToolSpec] {
        &self.hosted_tools
    }

    pub fn with_options(mut self, options: GenerateRequestOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_tool_choice(mut self, tool_choice: Option<ToolChoice>) -> Self {
        self.tool_choice = tool_choice;
        self
    }

    pub fn with_hosted_tools(mut self, hosted_tools: Vec<HostedToolSpec>) -> Self {
        self.hosted_tools = hosted_tools;
        self
    }
}
