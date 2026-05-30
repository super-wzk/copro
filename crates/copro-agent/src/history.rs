use copro_api::message::{InputMessage, Message, OutputMessage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentHistory {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    messages: Vec<Message>,
}

impl AgentHistory {
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self { messages }
    }

    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub(crate) fn messages_mut(&mut self) -> &mut Vec<Message> {
        &mut self.messages
    }

    pub fn push_input(&mut self, message: InputMessage) {
        self.messages.push(message.into());
    }

    pub(crate) fn push_output(&mut self, message: OutputMessage) {
        self.messages.push(message.into());
    }
}
