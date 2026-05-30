use super::SkillRuntime;
use super::tool::LOAD_SKILL_TOOL_NAME;
use copro_api::error::Result;
use copro_api::message::{InputContent, InputMessage, Message, OutputContent, OutputMessage};
use copro_api::request::GenerateRequest;
use std::sync::Arc;

/// Applies skill context to model requests before they are submitted.
#[derive(Clone)]
pub struct SkillRequestInjector {
    runtime: Arc<SkillRuntime>,
}

impl SkillRequestInjector {
    pub fn new(runtime: Arc<SkillRuntime>) -> Self {
        Self { runtime }
    }

    pub async fn prepare_request(&self, request: &mut GenerateRequest) -> Result<()> {
        prune_stale_skill_loads(&mut request.messages);

        if let Some(prompt) = self.runtime.available_skills_prompt().await? {
            let insertion_index = request
                .messages
                .iter()
                .take_while(|message| {
                    matches!(
                        message,
                        Message::Input(InputMessage::System(_))
                            | Message::Input(InputMessage::Developer(_))
                    )
                })
                .count();
            request.messages.insert(
                insertion_index,
                Message::developer(vec![InputContent::Text(prompt)]),
            );
        }
        Ok(())
    }
}

/// Remove loaded skill instructions from previous user turns before sending the
/// next model request.
///
/// The current turn must keep its `load_skill` call/result pair so the model can
/// consume the loaded instructions. Once a later user message exists, the old
/// pair is just historical scaffolding and can be omitted from context.
fn prune_stale_skill_loads(messages: &mut Vec<Message>) {
    let Some(last_user_index) = messages
        .iter()
        .rposition(|message| matches!(message, Message::Input(InputMessage::User(_))))
    else {
        return;
    };

    let mut original_index = 0usize;
    messages.retain_mut(|message| {
        let stale = original_index < last_user_index;
        original_index += 1;

        if !stale {
            return true;
        }

        match message {
            Message::Output(OutputMessage::Assistant(content)) => {
                content.retain(|item| {
                    !matches!(
                        item,
                        OutputContent::ToolCall(tool_call)
                            if tool_call.name == LOAD_SKILL_TOOL_NAME
                    )
                });
                !content.is_empty()
            }
            Message::Output(OutputMessage::Tool(result)) if result.name == LOAD_SKILL_TOOL_NAME => {
                false
            }
            _ => true,
        }
    });
}
