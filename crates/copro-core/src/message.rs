use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolResultStatus {
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InputContent {
    Text { text: String },
    Image { mime_type: String, data: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AssistantContent {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Map<String, Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Message {
    System {
        content: Vec<InputContent>,
    },
    User {
        content: Vec<InputContent>,
    },
    Assistant {
        content: Vec<AssistantContent>,
    },
    Tool {
        call_id: String,
        name: String,
        status: ToolResultStatus,
        content: Vec<InputContent>,
    },
}
