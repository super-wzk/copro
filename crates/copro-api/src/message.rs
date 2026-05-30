use derive_more::{Deref, Display, From, Into};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolResultStatus {
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InputContent {
    Text(String),
    Image(ImageContent),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Deref, Display, From, Into)]
#[serde(transparent)]
pub struct ToolCallId(String);

impl ToolCallId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ToolCallId {
    fn from(id: &str) -> Self {
        Self(id.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub name: String,
    pub status: ToolResultStatus,
    pub content: Vec<InputContent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageContent {
    Url { url: String },
    Data { mime_type: String, data: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OutputContent {
    Text(String),
    Thinking(String),
    Image(ImageContent),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Message {
    System(Vec<InputContent>),
    Developer(Vec<InputContent>),
    User(Vec<InputContent>),
    Assistant(Vec<OutputContent>),
    Tool(ToolResult),
}
