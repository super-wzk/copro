use super::SkillRuntime;
use super::format::format_skill_document;
use crate::tools::{ErasedTool, Tool};
use copro_agent::ToolRouter;
use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{InputContent, ToolCall, ToolResult, ToolResultStatus};
use copro_api::tool::ToolDefinition;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

pub(crate) const LOAD_SKILL_TOOL_NAME: &str = "load_skill";

const LOAD_SKILL_TOOL_DESCRIPTION: &str = "Load the full instructions for an available Agent Skill by name. Use this before applying a skill whose summary matches the current task.";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
struct LoadSkillInput {
    /// The exact skill name from the available skills list.
    pub name: String,
}

#[derive(Clone)]
struct LoadSkillTool {
    runtime: Arc<SkillRuntime>,
}

impl LoadSkillTool {
    fn new(runtime: Arc<SkillRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    type Input = LoadSkillInput;
    type Output = String;

    fn name(&self) -> &str {
        LOAD_SKILL_TOOL_NAME
    }

    fn description(&self) -> &str {
        LOAD_SKILL_TOOL_DESCRIPTION
    }

    async fn call(&self, input: Self::Input) -> std::result::Result<Self::Output, String> {
        self.runtime
            .load(&input.name)
            .await
            .map(|skill| format_skill_document(&skill))
            .map_err(|error| format!("failed to load skill `{}`: {error}", input.name))
    }
}

/// Tool router for skill-runtime tools only.
///
/// This router intentionally exposes and handles only skill-related tools. It
/// does not wrap, proxy, or compose unrelated tool routers; callers that need
/// multiple routers can compose them with [`crate::CompositeToolRouter`].
#[derive(Clone)]
pub struct SkillToolRouter {
    load_skill: LoadSkillTool,
}

impl SkillToolRouter {
    pub fn new(runtime: Arc<SkillRuntime>) -> Self {
        Self {
            load_skill: LoadSkillTool::new(runtime),
        }
    }
}

#[async_trait]
impl ToolRouter for SkillToolRouter {
    async fn definitions(&self) -> Result<Vec<ToolDefinition>> {
        Ok(vec![self.load_skill.definition()])
    }

    async fn execute(&self, call: ToolCall) -> Result<ToolResult> {
        let ToolCall {
            id,
            name,
            arguments,
        } = call;

        if name == LOAD_SKILL_TOOL_NAME {
            return match self
                .load_skill
                .call_content(serde_json::Value::Object(arguments))
                .await
            {
                Ok(content) => Ok(ToolResult {
                    call_id: id,
                    name,
                    status: ToolResultStatus::Success,
                    content,
                }),
                Err(error) => Ok(ToolResult {
                    call_id: id,
                    name,
                    status: ToolResultStatus::Error,
                    content: vec![InputContent::Text(error)],
                }),
            };
        }

        Ok(ToolResult {
            call_id: id,
            name: name.clone(),
            status: ToolResultStatus::Error,
            content: vec![InputContent::Text(format!("unknown skill tool: {name}"))],
        })
    }
}
