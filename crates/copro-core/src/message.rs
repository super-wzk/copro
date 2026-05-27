use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    System,
    Developer,
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
pub enum ImageContent {
    Url { url: String },
    Data { mime_type: String, data: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OutputContent {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    Image {
        image: ImageContent,
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
    Developer {
        content: Vec<InputContent>,
    },
    User {
        content: Vec<InputContent>,
    },
    Assistant {
        content: Vec<OutputContent>,
    },
    Tool {
        call_id: String,
        name: String,
        status: ToolResultStatus,
        content: Vec<InputContent>,
    },
}
