use super::SkillRuntime;
use copro_agent::AgentHook;
use copro_api::async_trait;
use copro_api::error::Result;
use copro_api::message::{InputContent, Message};
use copro_api::request::GenerateRequest;
use std::sync::Arc;

/// Hook that injects the available-skills prompt into every model request.
#[derive(Clone)]
pub struct SkillHook {
    runtime: Arc<SkillRuntime>,
}

impl SkillHook {
    pub fn new(runtime: Arc<SkillRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl AgentHook for SkillHook {
    async fn before_request(&self, request: &mut GenerateRequest) -> Result<()> {
        if let Some(prompt) = self.runtime.available_skills_prompt().await? {
            let insertion_index = request
                .messages
                .iter()
                .take_while(|message| matches!(message, Message::System(_) | Message::Developer(_)))
                .count();
            request.messages.insert(
                insertion_index,
                Message::Developer(vec![InputContent::Text(prompt)]),
            );
        }
        Ok(())
    }
}
